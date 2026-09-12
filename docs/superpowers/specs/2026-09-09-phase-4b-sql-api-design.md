# ORA-SQLLite Phase 4B SQL 前端与公共 API 设计

日期：2026-09-09
状态：已批准

## 1. 目标与阶段位置

Phase 4B 在 Phase 4A 已证明的系统目录、稳定对象 ID、隐藏 ROWID 和行编码之上，建立应用可见的数据库 API 与应用不可见的 SQL 编译前端。

本阶段完成后，应用能够：

- 创建或打开数据库并建立连接；
- 开始只读或写事务；
- 准备一条受支持的 SQL；
- 查看确定的参数和结果列元数据；
- 在准备阶段得到语法、名称、参数和静态类型错误。

本阶段不执行 SQL。`SELECT`、DDL 和 DML 都只产生与磁盘格式无关的强类型 `BoundStatement`。Phase 4C 才增加规划器、执行器、表数据读写、索引维护和约束执行。

Phase 5 继续负责 Oracle 数据类型声明属性、函数、序列、`DUAL`、`ROWNUM`、分页、空字符串转 NULL 和其他 Oracle 兼容语义。本阶段不得借解析便利提前实现这些行为。

## 2. 已选方案

采用分层编译管线：

```text
SQL 文本
  -> sqlparser-rs 私有解析结果
  -> ORA-SQLLite 自有 AST
  -> Oracle 标识符和参数规范化
  -> 快照目录名称绑定与静态类型检查
  -> ORA-SQLLite BoundStatement
```

选择该方案的理由：

- 复用成熟的纯 Rust 词法和基础语法实现；
- 第三方 AST 不进入公共 API、目录、规划器或执行器；
- Oracle 差异集中在项目自己的适配和规范化层；
- 解析依赖可以升级或替换，而不会改变 4C 的绑定输入；
- `prepare()` 在任何数据访问前给出确定错误。

不采用以下方案：

- 完全手写整个 SQL 解析器：范围和缺陷风险超过本阶段收益；
- 让 `sqlparser-rs` AST 贯穿系统：会把外部版本变化泄漏到核心边界；
- 执行时才绑定：错误过晚，模式变化和锁语义也更复杂。

## 3. 模块与依赖边界

新增模块：

```text
src/
  api/
    mod.rs
    database.rs
    options.rs
    connection.rs
    transaction.rs
    statement.rs
    metadata.rs
  sql/
    mod.rs
    parser.rs
    ast.rs
    name.rs
    normalize.rs
    bind.rs
    bound.rs
    types.rs
    limits.rs
```

职责如下：

- `api` 只暴露稳定应用概念，不暴露 Pager、目录记录或 SQL AST。
- `sql::parser` 是唯一可以引用 `sqlparser` 类型的生产模块。
- `sql::ast` 保存项目拥有的、未绑定且与磁盘无关的语法树。
- `sql::name` 和 `sql::normalize` 处理标识符、参数及规范化规则。
- `sql::bind` 只通过目录读取接口解析逻辑对象，不读取用户表页面。
- `sql::bound` 保存供 Phase 4C 使用的强类型语句和表达式。
- `sql::types` 定义 SQL 前端类型，不改变 Phase 4A 的磁盘列类型。
- `sql::limits` 在解析和树转换的每个增长点执行资源限制。

禁止反向依赖：`storage` 不得引用 `api`、`sql` 或 `sqlparser`；公共 API 不得出现 `sqlparser` 类型。

## 4. 依赖

### 4.1 SQL 解析

Phase 4B 使用：

```toml
sqlparser = { version = "=0.62.0", default-features = false, features = ["std"] }
```

只启用 `std`，不启用 `recursive-protection`、serde、visitor、BigDecimal、JSON 或派生 Dialect。`std` 已提供 `Parser::with_recursion_limit` 的递归计数；项目固定传入自身配置的 128 层默认上限。关闭额外的 `recursive-protection` 可以避免引入 `stacker/psm` 原生构建链，保持轻量和跨平台边界。模糊测试必须证明该选择不会把深输入变成进程栈溢出。

版本 0.62.0 采用 Apache-2.0 许可证。2026-09-09 已在 Windows MSVC 上以 Rust 1.85.0、`std`-only 配置和 128 层解析限制完成独立编译探针。crate 元数据未声明 `rust-version`，因此正式实现仍必须由 Windows、Linux 和 Rust 1.85 CI 门禁共同证明兼容性。

项目继续不提交 `Cargo.lock`。直接依赖使用精确版本；每次依赖升级使用独立提交，并重新运行完整语法语料库和 MSRV 门禁。

解析入口使用 `sqlparser` 的 Oracle Dialect 作为基础，但必须由项目私有适配器补足 Oracle 风格参数和项目允许列表。适配器先通过依赖的 tokenizer 获得带 span 的 token，再把源码中相邻的 `:` 与合法数字/名称 token 转成项目允许的 placeholder token，并通过 `Parser::with_tokens` 解析；字符串、引用标识符和注释内的冒号不得转换。合并 span 后仍以原 SQL 字节位置为准。依赖支持某种语法不等于 ORA-SQLLite 支持该语法。

### 4.2 数据库 UUID

公共 `Database::create()` 使用最小的操作系统随机源：

```toml
getrandom = { version = "=0.4.3", default-features = false, features = ["std"] }
```

getrandom 0.4.3 采用 MIT OR Apache-2.0 许可证并明确声明 Rust 1.85 MSRV。项目不引入通用随机数生成器或 UUID 格式化依赖。

创建前先调用 `getrandom::fill` 填充 16 字节，再把 byte 6 高四位设为 `0100`、byte 8 高两位设为 `10`，形成 RFC 4122 variant 的版本 4 UUID。随机源失败映射为 `Io`，并且必须发生在创建主文件或 WAL 之前。测试通过 crate 内部可注入 UUID 源证明固定字节、位规范化、随机源失败无文件副作用，以及连续创建得到不同数据库身份。

## 5. 公共 API

### 5.1 核心对象

公共层提供：

- `Database`：拥有可克隆的 Pager 句柄和数据库身份；`Clone + Send + Sync`。
- `Connection`：保存连接级会话状态；`Send`，不承诺 `Sync`，避免一个会话被并发修改。
- `ReadTransaction`：拥有固定 LSN 的只读快照。
- `WriteTransaction`：持有全数据库单写者资格，未提交离开作用域时回滚。
- `Statement`：不可变地拥有 `BoundStatement` 和公共元数据，可在线程间共享。
- `ParameterMetadata`：参数的规范名称、样式、序号和推断类型。
- `ColumnMetadata`：结果列名称、类型、可空性和来源信息。

Phase 4B 不公开 `Row`、游标、`execute()` 或 `query()`；这些名称只在存在真实 4C 行为时加入。

### 5.2 创建与打开

语义边界：

- `Database::create(path, options)` 只创建新数据库；路径存在时返回 I/O 分类错误，不覆盖。
- `Database::create` 在任何文件 I/O 前生成并规范化随机 UUID；随机源失败时主文件和 WAL 均不存在。
- `Database::open(path, options)` 只打开已有数据库；路径不存在时失败，不隐式创建。
- `Options` 明确选择读写或只读模式，并承载已有页、缓存、数据库和 WAL 限制及本阶段 SQL 限制。
- 页大小只在创建时生效；打开时必须服从磁盘格式。
- 每个 `Database` 保留磁盘 UUID，供未来执行时检查 `Statement` 所属数据库。

### 5.3 连接与事务

建议接口形状：

```rust
let db = Database::open("app.db", Options::default())?;
let mut conn = db.connect()?;

let statement = conn.prepare(
    "SELECT ID, NAME FROM USERS WHERE ID = :1"
)?;

for parameter in statement.parameters() {
    // 读取位置、名称和推断类型
}
```

事务规则：

- `Connection::begin_read(&mut self)` 返回 `ReadTransaction<'_>`。
- `Connection::begin_write(&mut self)` 返回 `WriteTransaction<'_>`。
- Rust 可变借用保证同一连接最多有一个活动事务。
- 多个连接可以同时持有只读事务；写事务继续服从 Phase 3 的全库单写者规则。
- `ReadTransaction` 离开作用域时释放快照。
- `WriteTransaction` 只有显式 `commit()` 才提交；其他退出路径回滚。
- 数据库以只读模式打开时，开始写事务返回 `Unsupported`。

Phase 4B 不通过 SQL 文本执行 `BEGIN`、`COMMIT` 或 `ROLLBACK`；它们可以被解析和绑定，行为留给 4C。Rust 事务 API 是本阶段真实可用的存储事务边界。

### 5.4 准备语句

- `Connection::prepare(sql)` 使用一个短期最新目录快照，返回后释放快照。
- `ReadTransaction::prepare(sql)` 使用该事务固定的目录快照。
- `WriteTransaction::prepare(sql)` 使用写事务的基准目录视图；Phase 4B 没有未提交 DDL。
- `Statement` 拥有 SQL 编译结果，不借用连接或目录快照，可以在连接之外保存。
- `Statement` 记录数据库 UUID、引用对象 ID、表模式版本、参数元数据和结果列元数据。
- `Statement` 不保存目录页引用、B+Tree 根或第三方 AST。

在 Phase 4C 中执行前必须比较数据库 UUID、对象 ID 和模式版本。任何不匹配返回 `SchemaChanged`，不自动重新解析或绑定。

Phase 4B 在 `sql::bound` 中实现并测试 crate 内部的模式验证入口。它只读取当前 `CatalogImage`，不规划或执行语句；Phase 4C 必须在每次执行前调用该入口。

## 6. 单语句与词法规则

每次 `prepare()` 只接受一条语句：

- 允许空白、行注释、块注释和一个可选末尾分号；
- 空输入返回 `Syntax`；
- 第二条非空语句返回 `Syntax`；
- 不支持嵌套块注释；
- 字符串字面量使用单引号，内部单引号用两个单引号表示；
- 标识符使用双引号；不接受方括号或反引号作为 Oracle 标识符引用；
- 数字字面量按项目自有规则重新解析和验证，不能直接信任第三方 AST 中的字符串。

输入先检查字节上限，再词法分析；token 数检查发生在建立项目 AST 前。第三方错误位置转换为项目自己的 SQL 位置。

## 7. 标识符与对象命名

### 7.1 规范化

标识符保存原始跨度和规范化值：

- 未加双引号的标识符按 ASCII 规则转为大写；
- 加双引号的标识符保留 UTF-8 内容和大小写；
- 空引用标识符非法；
- 规范化后最大长度为 128 UTF-8 字节；
- 未引用保留字不能作为普通标识符；
- 名称规范化后才构造 Phase 4A `CatalogName`。

大小写折叠仅处理 SQL 标识符允许的 ASCII 字母。非 ASCII 未引用标识符不做 Unicode locale 或 NLS 折叠。

### 7.2 名称空间

首版嵌入式数据库没有用户或 schema：

- 表和索引名称必须是不带 schema 的单段对象名；
- `table.column` 和 `alias.column` 是允许的两段列引用；
- `schema.table`、数据库链接和三段以上名称返回 `Unsupported`；
- 表别名存在时隐藏原表名；
- 同一查询块中的别名必须唯一。

## 8. 参数

支持两类 Oracle 风格参数：

- 位置参数：`:1` 到 `:65535`；
- 命名参数：冒号后接未引用标识符形式的名称。

确定性规则：

- 一条语句不能混用位置和命名参数；
- `:0`、前导零位置参数和超过 65535 的编号非法；
- 位置编号必须形成从 1 开始的连续集合；
- 重复位置编号引用同一个逻辑参数；
- 命名参数按未引用标识符规则规范化为大写；
- 大小写不同但规范化后相同的命名参数是同一个逻辑参数；
- 不支持带引号的参数名称；
- `Statement::parameters()` 按位置编号或首次出现顺序稳定返回。

参数类型从插入/更新目标列、比较对象、算术运算和函数签名中收集约束。同一参数的全部约束必须存在公共兼容类型，否则 `prepare()` 返回 `Type`。没有任何类型上下文时类型为 `Unknown`，Phase 4C 执行时要求调用方提供明确类型。

Phase 4B 不接收参数值，也不检查缺少或多余的运行时值。

## 9. 自有 AST

项目 AST 必须：

- 拥有全部字符串和集合；
- 为用户可定位节点保存项目自己的 `SqlSpan`；
- 不保存 `sqlparser` 类型、引用或 trait object；
- 对每个允许的语法变体使用显式 enum；
- 在构造时检查集合大小和递归深度；
- 不实现磁盘编码，也不作为长期兼容格式。

### 9.1 语句范围

Phase 4B AST 接受：

- `CREATE TABLE`、`DROP TABLE`；
- `CREATE [UNIQUE] INDEX`、`DROP INDEX`；
- `SELECT`、`INSERT`、`UPDATE`、`DELETE`；
- `BEGIN`、`COMMIT`、`ROLLBACK`。

不接受过程块、匿名 PL/SQL、MERGE、TRUNCATE、GRANT、用户、schema、tablespace 或网络数据库语法。

### 9.2 查询范围

接受：

- 显式投影和 `*`、`table.*`；
- `DISTINCT`；
- 单表和显式 `INNER JOIN`、`LEFT [OUTER] JOIN ... ON`；
- `WHERE`；
- `GROUP BY` 和 `HAVING`；
- `ORDER BY`，包括列、查询别名和正整数投影序号；
- 聚合 `COUNT`、`SUM`、`AVG`、`MIN`、`MAX`。

拒绝：

- 子查询、CTE、递归查询；
- UNION、INTERSECT、MINUS 和其他集合运算；
- 窗口函数和 `QUALIFY`；
- NATURAL、USING、CROSS、FULL 和 RIGHT JOIN；
- PIVOT、MODEL、CONNECT BY 和层次查询；
- SELECT FOR UPDATE；
- OFFSET、FETCH、LIMIT 和 ROWNUM；
- `DUAL` 的特殊处理。

### 9.3 表达式范围

接受：

- NULL、布尔、整数、十进制、指数浮点、文本和 `X'ABCD'` 形式的偶数位十六进制二进制字面量；
- 列引用和参数；
- 一元正负号和 `NOT`；
- 加、减、乘、除和取余；
- `=`, `<>`, `!=`, `<`, `<=`, `>`, `>=`；
- `AND`, `OR`；
- `IS [NOT] NULL`；
- 非空 `IN` 值列表；
- `[NOT] BETWEEN`；
- `[NOT] LIKE`；
- 括号表达式；
- 上述五个聚合函数。

标量函数、字符串连接、日期算术和显式/隐式 Oracle 转换属于 Phase 5。CASE、CAST 和所有子查询表达式在本阶段返回 `Unsupported`。

## 10. 名称绑定

绑定器只依赖一个不可变 `CatalogImage` 和本查询块的作用域。

### 10.1 表和列

- FROM 项先绑定表名，再建立别名作用域。
- 表引用变为稳定 `TableId` 和 `SchemaVersion`。
- 限定列解析为唯一表的稳定 `ColumnId`。
- 非限定列必须在当前查询块中唯一；零个匹配为未知列，多个匹配为歧义列。
- `*` 在绑定时按 FROM 顺序和表内 ordinal 展开。
- `table.*` 只展开唯一匹配的表或别名。
- 绑定结果保存逻辑 ID 和列 ordinal，不保存名称查找作为执行依据。

### 10.2 查询处理顺序

逻辑绑定顺序为：

1. FROM 和 JOIN 输入；
2. JOIN ON；
3. WHERE；
4. GROUP BY；
5. 聚合和 HAVING；
6. SELECT 投影及别名；
7. DISTINCT；
8. ORDER BY。

SELECT 别名只在 ORDER BY 可见，不进入 WHERE、GROUP BY 或 HAVING。ORDER BY 正整数按一开始引用投影位置；零、越界和非整数位置返回 `Bind`。

聚合查询中，未聚合的投影、HAVING 和 ORDER BY 表达式必须与一个 GROUP BY 表达式结构等价；没有 GROUP BY 时只能引用聚合或常量。聚合不能嵌套。

### 10.3 DDL 和 DML

- CREATE 对象在准备快照中必须不存在；执行时 4C 必须再次验证名称仍为空闲。
- DROP 对象绑定为稳定对象 ID；同名对象被删除后重建不能满足旧绑定。
- INSERT 未列出列时按目录 ordinal 建立目标列列表；列名不能重复。
- UPDATE 目标列不能重复，赋值右侧在旧行作用域求值。
- DELETE 和 UPDATE 的 WHERE 使用目标表作用域。
- 索引列必须属于目标表、不得重复；表达式索引不在本阶段范围。
- 约束引用在自有 AST 和绑定结果中表示，实际目录格式及执行由 4C 定义。

## 11. 类型系统与静态检查

SQL 前端类型为：

```text
Null, Unknown, Int64, Decimal, Float64, Text, Blob,
Date, Timestamp, Bool
```

它与公共 `Value` 和 Phase 4A `ColumnType` 可以显式映射，但不是磁盘枚举。

本阶段只采用保守、无歧义规则：

- NULL 或 Unknown 可以从另一侧上下文取得类型；
- Int64 可以安全提升到 Decimal；
- Decimal 与 Float64 不隐式互转；
- 条件必须为 Bool；
- LIKE 两侧必须为 Text；
- BETWEEN 和 IN 的被比较值必须存在统一兼容类型；
- 排序比较只接受相同类型或 Int64/Decimal 组合；
- Blob 只支持相等和不等比较；
- COUNT 返回非空 Int64；
- SUM(Int64/Decimal) 和 AVG(Int64/Decimal) 返回可空 Decimal；
- SUM/AVG(Float64) 返回可空 Float64；
- MIN/MAX 返回与输入相同的可空类型；
- 其他聚合输入在准备阶段返回 Type。

空文本在 Phase 4B 仍是 Text，不转为 NULL。Oracle 空字符串规则只在 Phase 5 的兼容层启用。

## 12. DDL 类型声明边界

自有 AST 无损保存类型名称、参数和修饰符，但绑定只接受能够由 Phase 4A V1 目录完整表达的核心类型：

```text
INT64, DECIMAL, FLOAT64, TEXT, BLOB, DATE, TIMESTAMP, BOOL
```

这些名称用于 Phase 4C 的通用关系能力，不宣称 Oracle 类型兼容。

`NUMBER(p,s)`、`VARCHAR2(n)`、`CHAR(n)`、`RAW(n)`、`TIMESTAMP WITH TIME ZONE` 等可以被解析为类型语法，但绑定必须返回 `Unsupported`，直到 Phase 5 增加能够持久化和执行其声明属性的新目录记录版本。不得忽略长度、精度、标度或时区修饰符后继续创建表。

## 13. BoundStatement 合同

`BoundStatement` 是 crate 内部、不可序列化的 Phase 4C 输入，包含：

- 语句种类和项目自有源跨度；
- 数据库 UUID；
- 每个引用表的 `TableId` 和 `SchemaVersion`；
- 每个列引用的 `ColumnId`、ordinal 和静态类型；
- 强类型绑定表达式；
- 规范化参数 ID 及推断类型；
- 确定的输出列名称、类型、可空性和可选来源 ID；
- DDL 创建名称或 DROP 的稳定对象 ID；
- 4C 规划所需、但与物理页无关的约束描述。

它不得包含：

- SQL 解析依赖的 AST；
- Pager、页号、B+Tree 根或 WAL 状态；
- 数据行或参数值；
- 执行计划、索引选择或成本信息；
- Phase 5 Oracle 改写结果。

Phase 4C 在执行前验证：

1. 当前数据库 UUID 相同；
2. 每个稳定对象 ID 仍存在且种类相同；
3. 每个表模式版本与准备时相同；
4. CREATE 名称仍未占用。

失败返回 `SchemaChanged`，调用方重新 `prepare()`。系统不得静默绑定到同名新对象，也不自动重编译。

上述检查函数属于 Phase 4B 的 `BoundStatement` 合同并在本阶段完成。Phase 4C 的责任是无条件调用它，而不是重新实现另一套校验。

## 14. 错误模型与 SQL 位置

新增稳定错误码：

```rust
SchemaChanged = 1150
```

现有分类保持：

- `Syntax`：词法、语法、空输入和多语句；
- `Bind`：对象或列不存在、歧义、作用域和参数规则；
- `Type`：表达式、参数上下文或核心类型不兼容；
- `Unsupported`：语法有效但不在阶段范围；
- `Busy`、`Transaction`、`Io`、`Corruption`、`Internal`：保留存储层语义。

`DbError` 增加可选项目类型 `SqlLocation`：

```text
byte_offset, line, column
```

字节偏移从 0 开始；行和列从 1 开始。CRLF 和 LF 各计为一个换行，列按 Unicode 标量计数，tab 计一个标量；机器处理应以字节偏移为权威。位置只定位输入 SQL，不保存第三方 span 类型。没有输入位置的存储或事务错误保持 `None`。

所有 `sqlparser` 错误都转换为项目错误。生产路径不得因未知 AST 变体、深嵌套、无效数值或非法 UTF-8 边界 panic。

## 15. 资源限制

默认 SQL 限制：

| 资源 | 默认上限 |
|---|---:|
| SQL UTF-8 字节 | 1,048,576 |
| 非空白 token | 100,000 |
| 自有 AST 节点 | 100,000 |
| AST/表达式嵌套深度 | 128 |
| 逻辑参数 | 65,535 |
| 单查询块 JOIN | 64 |
| 表列数 | 沿用 1,000 |

规则：

- 配置值必须为正且不能超过实现的硬边界；
- 加法、乘法和容量预估使用 checked arithmetic；
- 到达上限返回受控错误，不做部分绑定；
- 深度检查必须在递归下降或项目 AST 构造前生效；
- `Parser::with_recursion_limit` 和项目 AST 深度检查必须使用同一配置上限；
- `prepare()` 不得修改主文件、WAL、目录或连接事务状态。

## 16. 测试设计

### 16.1 解析适配与黄金 AST

为每种受支持语句和表达式保存手工期望的自有 AST。覆盖注释、可选分号、字符串转义、引用标识符和所有拒绝项。检查第三方库接受但项目不支持的语法返回 `Unsupported`。

加入升级哨兵测试，确认生产 `sqlparser` 引用只存在于解析模块，公共类型和 `BoundStatement` 不包含第三方类型。

### 16.2 名称和参数

覆盖：

- 未引用大小写折叠和引用名称区分；
- 127、128、129 字节名称；
- 非 ASCII UTF-8 字节边界；
- 保留字、空名称和非法限定层级；
- 表别名隐藏、未知列和歧义列；
- `*` 与 `table.*` 的稳定展开顺序；
- 位置和命名参数、重复引用、混用、编号缺口、前导零和上限；
- 参数多上下文类型归并与冲突。

### 16.3 真实目录绑定

测试必须通过 Phase 4A 的真实目录写入和快照读取建立表，不使用另一个宽松的假目录语义。覆盖：

- 多表 INNER/LEFT JOIN；
- WHERE、GROUP BY、HAVING、DISTINCT 和 ORDER BY；
- INSERT 列映射和 UPDATE 目标；
- DDL、索引和约束引用；
- 旧快照与新目录提交隔离；
- 删除并重建同名表仍保留不同对象身份。

### 16.4 模式变化

准备语句后执行以下变化：

- 删除表；
- 删除并重建同名表；
- 增删列或改变 ordinal；
- 提升表模式版本；
- 在另一数据库使用语句。

每种情况都必须在 Phase 4B 提供的内部模式校验入口得到 `SchemaChanged`。未变化的无关表不得使语句失效；Phase 4C 后续只复用这个已证明入口。

### 16.5 公共 API 和只读证明

- README 级示例作为 doctest 或集成编译测试；
- 编译期证明 Database 可共享、Connection 会话不并发共享、事务借用阻止同连接重入；
- 写事务未提交 Drop 后无文件变化；
- `prepare()` 前后主文件和 WAL 字节完全一致；
- 连接准备释放短期快照，事务准备保留事务原有快照；
- 只读数据库可以 prepare，不能开始写事务。

### 16.6 属性、鲁棒性与回归

- 固定种子生成标识符、参数、表达式和查询结构；
- 随机字节、随机合法 UTF-8、每个截断点和极深嵌套不得 panic；
- 资源限制的上限减一、正好上限和上限加一均有测试；
- Windows、Linux、Rust 1.85、格式和 Clippy 门禁通过；
- Phase 1–4A 全部常规、恢复、目录和行损坏测试继续通过；
- 专用 CI 运行解析/绑定语料库和资源限制矩阵。

## 17. 验收标准

进入 Phase 4C 前必须全部满足：

- Database、Connection、只读/写事务和 Statement 公共 API 可真实使用；
- `prepare()` 对本规格全部 SQL 产生项目自有 `BoundStatement`；
- 标识符、作用域、参数、聚合和静态类型规则有确定行为；
- 参数和结果列元数据稳定并与目录 ordinal 一致；
- 模式依赖使用数据库 UUID、稳定对象 ID 和模式版本表达；
- crate 内部模式校验入口能够在不执行 SQL 的情况下确定返回 `SchemaChanged`；
- 解析和准备不产生任何数据库或 WAL 写入；
- 第三方 AST、错误和 span 不泄漏到公共 API 或绑定结果；
- 所有资源边界和错误分类经过负向测试；
- sqlparser 0.62.0 在 Windows、Linux 和 Rust 1.85 门禁通过；
- Phase 1–4A 回归套件通过；
- 源码中不存在执行计划、执行器、用户行 CRUD、索引维护、约束执行或 Phase 5 Oracle 行为。

## 18. Phase 4C 交接

Phase 4C 可以依赖：

- SQL 已经转换为项目拥有的强类型绑定表示；
- 所有表列引用都是稳定逻辑 ID；
- 参数列表、参数类型和输出列模式已经确定；
- 基础聚合和连接语义已经静态验证；
- 预执行模式校验返回明确 `SchemaChanged`；
- 公共事务对象已经映射到 Phase 3 的快照和单写者边界。

Phase 4C 负责把 `BoundStatement` 转换为规则计划和拉取式执行算子，并将目录、表树、索引和约束变化置于正确 WAL 事务中。它不得绕回第三方 AST 或重新按字符串解析对象。

## 19. 结论

Phase 4B 采用“第三方解析器只做私有前端、自有 AST 冻结语义、快照目录完成绑定、公共 API 只暴露真实能力”的设计。该边界保持 SQLite 式嵌入和轻量运行时，同时为 Phase 4C 提供稳定、可测试且不依赖磁盘页细节的 SQL 输入。
