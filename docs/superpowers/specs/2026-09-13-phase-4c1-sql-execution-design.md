# ORA-SQLLite Phase 4C1 SQL 执行设计

日期：2026-09-13
状态：设计已在对话中逐节批准，等待书面规格复核

## 1. 目标

Phase 4C1 在 Phase 4B 的强类型 `BoundStatement` 与 Phase 4A/Phase 3 的目录、行存储和 WAL 事务之间加入真实 SQL 执行能力，形成首个完整的嵌入式 CRUD 闭环：

- 执行无约束的 `CREATE TABLE` 与 `DROP TABLE`；
- 执行单行 `INSERT`、`UPDATE`、`DELETE`；
- 执行 Phase 4B 已接受的全部 `SELECT` 范围；
- 提供自动提交、SQL `BEGIN`/`COMMIT`/`ROLLBACK` 和现有 Rust 事务 API；
- 提供执行时参数、流式结果行和影响行数；
- 保证模式验证、语句级原子性、事务隔离、WAL 恢复和受控资源使用。

Phase 4C1 采用规则计划和拉取式执行算子，不引入成本优化器、字节码虚拟机或第二套存储协议。

## 2. 阶段拆分与非目标

Phase 4C 被拆为两个可独立验收的阶段：

- Phase 4C1：真实 DDL、完整已绑定 SELECT、基础 DML 和事务执行；
- Phase 4C2：二级索引及 `PRIMARY KEY`、`UNIQUE`、`FOREIGN KEY`、`CHECK` 等约束的持久化和维护。

Phase 4C1 明确不实现：

- `CREATE INDEX`、`DROP INDEX` 和索引选择；
- 任何已声明约束的持久化或执行；
- 成本估算、连接重排、统计信息和自适应优化；
- 查询中间结果的磁盘临时文件或外部排序；
- PL/SQL、过程对象、网络协议、用户、schema 或权限；
- Oracle 类型声明属性、函数、序列、`DUAL`、`ROWNUM`、分页、空字符串转 NULL、NLS 或 ORA 错误映射；
- `ALTER TABLE`、`MERGE`、`TRUNCATE`、子查询、集合运算、窗口函数、`SAVEPOINT` 或嵌套事务；
- 1000 万行或 10 GB 数据集作为每次 CI 的强制门槛。

Phase 4B 已经能够绑定的索引或约束语句仍可保留为 `Statement`。Phase 4C1 执行此类语句时，在任何存储修改之前返回 `Unsupported`；自动提交执行还必须在获取写者之前返回。显式事务已经独立持有写者，不为此语句重新获取。不得静默忽略、部分保存或声明一个尚未执行的约束。

## 3. 已批准的核心选择

- 执行 Phase 4B 已接受的全部 SELECT，而不是只实现临时的单表子集。
- DDL 使用 SQLite 风格事务语义，可随显式事务提交或回滚；不采用 Oracle DDL 隐式提交。
- 未显式 `BEGIN` 时，每条 DDL/DML 自动形成一个独立事务。
- 同时保留 SQL 事务控制与 Rust 事务对象，两者不能嵌套。
- 写者冲突立即返回 `Busy`，Phase 4C1 不增加内部等待超时。
- 查询通过拉取式 `Rows` 逐行返回，不一次性返回全部结果。
- 参数值在每次 `query`/`execute` 调用时提供；`Statement` 保持不可变并可复用。
- 普通语句错误只回滚当前语句；致命存储错误使显式事务只能回滚。
- 排序、分组和去重使用有界内存，超限返回 `ResourceLimit`，不创建临时文件。
- 常规 CI 以正确性、事务、恢复和并发为强制门槛；10 万行只作为有界烟测。

## 4. 分层架构

新增的执行路径为：

```text
SQL text
  -> Phase 4B parser / owned AST / binder
  -> BoundStatement
  -> Phase 4C1 rule planner
  -> immutable Plan
  -> execution context + parameter frame
  -> pull operators or DDL/DML sink
  -> Phase 4A catalog / row codec / table B+Tree
  -> Phase 3 WriteBatch / ReadSnapshot / WAL
```

建议的源代码边界：

```text
src/sql/plan/
  mod.rs             statement-to-plan conversion
  expr.rs            immutable executable expressions
  query.rs           query plan nodes
  mutation.rs        DDL/DML plans

src/sql/exec/
  mod.rs             execution contexts and dispatch
  params.rs          parameter normalization and type checks
  expr.rs            scalar and aggregate evaluation
  operator.rs        pull-operator contract
  scan.rs            one-row and table scans
  join.rs            inner and left nested-loop joins
  aggregate.rs       grouped and scalar aggregates
  distinct.rs        bounded-memory duplicate removal
  sort.rs            bounded-memory ordering
  mutation.rs        DDL/DML sinks and statement rollback guard

src/api/
  row.rs             Row and Rows
  params.rs          Parameters and typed NULL
  result.rs          ExecutionResult
```

具体文件可以在实现计划中按现有模块大小微调，但下列边界不得改变：

- 规划器只接收项目自有 `BoundStatement`，不得读取第三方 AST 或重新解析 SQL 文本；
- Plan 不保存目录页、B+Tree 页面、快照、原始锁或第三方类型；
- 执行器只能通过 Phase 4A/Phase 3 的受控接口访问持久化数据；
- 公共 API 不暴露 Plan、物理页面、对象内部 ID 或存储锁。

拉取算子的内部合同为等价于 `next(&mut self, &mut ExecutionContext) -> Result<Option<ExecRow>>` 的短期借用。算子持有游标状态，但不长期借用 ReadSnapshot 或 WriteBatch；因此一个 JOIN 可以顺序推进外侧和内侧扫描，自动提交 Rows 也可以安全地同时拥有快照与算子树。

## 5. Statement 与规划

`prepare()` 在 Phase 4B 绑定成功后生成一次不可变规则计划。`Statement` 通过 `Arc` 拥有绑定结果、计划和公共元数据，继续满足可克隆、可跨线程共享且自身不可变的合同。

计划节点至少包括：

- `OneRow`：没有 `FROM` 的常量查询；
- `TableScan`：通过稳定 `TableId` 和列 ordinal 读取 ROWID 顺序的行；
- `NestedLoopJoin`：按 SQL 源顺序执行 INNER 或 LEFT JOIN；
- `Filter`：用于 JOIN ON、WHERE 和 HAVING；
- `Aggregate`：标量或分组的 COUNT、SUM、AVG、MIN、MAX；
- `Project`：生成已绑定投影列；
- `Distinct`：对完整投影行去重；
- `Sort`：执行绑定后的排序表达式、方向和 NULL 位置；
- `Insert`、`Update`、`Delete`、`CreateTable`、`DropTable`；
- `Begin`、`Commit`、`Rollback`；
- `DeferredUnsupported`：保留 Phase 4B 已绑定、但属于 4C2 的索引或约束执行请求。

计划生成是确定性的，不读取运行时行数，不选择索引，也不改变 JOIN 顺序。Plan 中的每个可能产生运行时 SQL 错误的表达式保存由原 SQL span 转换得到的项目自有 `SqlLocation`；不需要在 Statement 中保留第三方 span。

## 6. 公共执行 API

API 形状采用执行上下文持有资源、Statement 保持不可变的方式：

```rust
let statement = connection.prepare(
    "SELECT ID, NAME FROM USERS WHERE ID = :1 ORDER BY NAME"
)?;

let mut rows = connection.query(
    &statement,
    Parameters::positional(&[Value::Int64(42).into()]),
)?;

while let Some(row) = rows.next()? {
    // Row 拥有本行 Value；按 ordinal 读取。
}
```

公共对象：

- `Parameters`：空、位置和命名三种形式；只在一次调用中借用或拥有参数集合；
- `ParameterValue`：普通 `Value` 或显式类型的 NULL；普通 Value 提供直接转换；
- `Row`：拥有 `Box<[Value]>` 或等价的有界行值集合；
- `Rows`：绑定一次执行上下文的拉取式游标；
- `ExecutionResult`：至少提供 `rows_affected() -> u64`。

INSERT、UPDATE 和 DELETE 返回实际影响行数；DDL 与事务控制返回 0。4C1 不公开隐藏 ROWID、生成键或 RETURNING 结果。Row 至少提供按零基 ordinal 读取值以及完整值切片，列名和类型继续从 Statement 的 ColumnMetadata 获得。

方法职责：

- `Connection::query`：自动创建读快照，或使用连接中的活动 SQL 写事务；
- `Connection::execute`：执行自动提交写语句或 SQL 事务控制；
- `ReadTransaction::query`：只在固定快照上执行查询；
- `WriteTransaction::query`：读取当前批次，包括本事务未提交修改；
- `WriteTransaction::execute`：在当前批次执行 DDL/DML；提交和回滚仍由事务对象的方法完成。

`Connection::prepare` 从 Phase 4B 的 `&self` 调整为 `&mut self`。这样它才能在 SQL 显式事务中读取活动 WriteBatch 的目录视图，并与尚未释放的 Rows 保持同一套编译期互斥规则。这是 0.1 阶段有意的 API 演进，不通过内部可变性绕过会话所有权。

`query` 只接受产生行的 Statement，`execute` 不接受查询 Statement；误用返回 `Type`。SQL `BEGIN`/`COMMIT`/`ROLLBACK` 只能由 `Connection::execute` 执行，在 Rust 事务对象中返回 `Transaction`。

`Rows` 的生命周期阻止同一个执行上下文在游标未释放时被再次可变使用。丢弃未读完的 `Rows` 立即终止后续算子工作并释放其快照或上下文借用，不提交额外修改。

现有 BTreeCursor 长期持有 `&mut PageSource`，既不能放入同时拥有自动提交快照的 Rows，也不能让嵌套循环的两个扫描顺序借用同一快照。4C1 增加或重构为自持有的扫描状态：它保存范围、当前叶信息和继续键，每次 next 只短期借用 PageSource。该调整不改变 B+Tree 磁盘格式、键顺序、边界或损坏检查；原有扫描测试必须继续通过。

## 7. 参数合同

执行启动时先完成全部参数检查，任何失败都发生在打开行游标、取得写者资格或修改存储之前。

规则为：

- 位置参数必须精确覆盖 `1..N`；
- 命名参数使用 Phase 4B 的未引用标识符规范化规则，调用方名称可以省略冒号；
- 缺失、多余或规范化后重复的参数返回 `Bind`；
- 一条 Statement 的运行时参数样式必须与准备元数据一致；
- 值类型必须与推断类型一致，只允许 `Int64` 到 `Decimal` 的安全提升；
- `Value::Null` 在元数据已有具体类型时取得该类型；
- 参数类型在绑定时为 Unknown 且运行时值非 NULL 时，以 Value 变体确定本次类型；
- Unknown 参数接收 NULL 时，调用方必须使用 `ParameterValue::typed_null(DataType)`；
- typed NULL 不得使用 `Null` 或 `Unknown` 作为其显式目标类型。

参数集合只在 `query`/`execute` 调用期间借用；执行器在返回 Rows 前把规范化后的值复制到本次执行拥有的参数帧。参数帧在本次执行期间不可变，重复出现的同一逻辑参数只保存一个值，所有表达式引用该规范槽位。参数值的拥有字节计入查询工作内存预算。

## 8. 查询执行语义

每次 `query()` 按固定顺序执行：

1. 检查 API 与参数合同；
2. 对当前事务视图调用 Phase 4B 的 UUID、对象 ID 和模式版本验证入口；
3. 创建本次执行的参数帧和算子状态；
4. 由 `Rows::next()` 拉取结果。

逻辑算子顺序为：

```text
OneRow/TableScan
  -> JOIN ON
  -> WHERE
  -> GROUP BY and aggregate
  -> HAVING
  -> projection
  -> DISTINCT
  -> ORDER BY
```

行为规则：

- 没有 FROM 的查询从 `OneRow` 产生恰好一行输入；
- TableScan 解码当前快照或批次中的表行，按隐藏 ROWID 递增扫描；
- 没有 ORDER BY 时不形成公共顺序保证，即使首个实现通常观察到 ROWID 顺序；
- INNER JOIN 只返回 ON 为 TRUE 的组合；LEFT JOIN 在无匹配时为右侧列生成 NULL；
- 4C1 使用嵌套循环连接，不做哈希连接、合并连接或源重排；
- WHERE 和 HAVING 只有 TRUE 保留行，FALSE 与 NULL 都丢弃；
- 聚合不能嵌套，分组与投影合法性继续由 Phase 4B 保证；
- COUNT 返回非空 Int64；空输入的 SUM、AVG、MIN、MAX 返回 NULL；
- DISTINCT 作用于完整投影行；
- ORDER BY 服从已绑定投影位置、别名或表达式；
- ASC 默认 NULLS LAST，DESC 默认 NULLS FIRST；显式 NULLS FIRST/LAST 优先；
- 文本比较区分大小写并按 UTF-8 字符串的既定逻辑顺序比较，不实现 locale 或 Oracle NLS；
- Blob 只进行 Phase 4B 已允许的相等或不等判断；
- LIKE 区分大小写，`%` 匹配任意序列，`_` 匹配一个 Unicode 标量；
- Date 与 Timestamp 不做 Phase 5 日期算术或隐式转换。

排序采用稳定的同类型总顺序：Bool 为 FALSE 后 TRUE，数值按数值大小，Text 按 Unicode 标量序列，Blob 按无符号字节序列，Date 按日历时间字段。Timestamp 按保存的日期、微秒、offset 是否存在及 offset 数值组成的字段元组比较，不在 4C1 做时区归一化。Int64 与 Decimal 比较前把 Int64 安全提升为 Decimal。DISTINCT 和 GROUP BY 把多个 NULL 视为同一键，这不改变普通 `NULL = NULL` 仍为 UNKNOWN 的规则。

Phase 4C1 的 SQL 层只接受有限 Float64。Float 字面量已由 Phase 4B 检查；运行时参数、持久行和算术结果出现 NaN 或正负无穷时返回 Type。正零和负零在比较、分组与 DISTINCT 中视为同一个值。底层 RowCodec 仍可无损保存全部 IEEE 位模式，但 SQL 执行器不为非有限值定义跨平台查询语义。

SQL 表达式采用标准 NULL 传播和三值布尔逻辑。算术溢出、除零和无法执行的值组合返回 `Type`，并使用计划表达式保存的 SQL 位置。表达式深度继续服从 Phase 4B 上限；所有错误路径必须受控返回，不得 panic。

## 9. 查询工作内存

普通扫描、过滤、投影和嵌套循环 JOIN 保持拉取式。分组、DISTINCT 和排序可以物化中间状态，但共享同一次查询的工作内存预算。

`Options` 增加查询工作内存设置，默认 64 MiB。预算至少计算：

- 被物化行的 Value 本体；
- Text 与 Blob 的实际字节；
- 分组键、去重键和排序键；
- 本次执行拥有的参数值；
- 容器节点的保守固定开销。

实现不需要声称等于分配器的精确字节数，但同一输入与配置必须得到确定的预算判断。所有加法和乘法使用受检查运算。下一项将超出预算时，在保留该项前返回 `ResourceLimit`，并释放本查询已物化的状态。

Phase 4C1 不创建临时数据库、旁路文件或可恢复的排序文件。

## 10. DDL 执行

4C1 只执行没有任何列级或表级约束的 `CREATE TABLE`，以及 `DROP TABLE`。

CREATE TABLE 在一个 WriteBatch 内完成：

1. 重新验证创建名称在当前目录视图中仍为空闲；
2. 分配稳定对象 ID、列 ID 和表 B+Tree 根；
3. 写入 TableDef、ColumnDef 和对象名称记录；
4. 完成目录候选验证；
5. 由外层自动提交或显式事务决定是否发布。

DROP TABLE 通过绑定的 TableId 定位对象，在同一批次中删除全部目录记录并释放表树。不得按名称重新绑定另一个同名表。

DDL 在显式写事务中立即对该事务后续的 `prepare`、`query` 和 `execute` 可见，提交前对其他连接不可见。DDL 回滚恢复原目录和表树。长读快照继续依赖 Phase 3 的版本保留观察旧目录和旧表页。

## 11. DML 执行

INSERT：

- 先计算全部值、完成参数和列类型检查；
- 未指定的目标列使用 NULL；4C1 没有 DEFAULT 执行语义；
- 所有值合法后才分配 ROWID；
- ROWID、目录中的 `next_rowid` 和编码行在同一 WriteBatch 中修改；
- 成功影响行数为 1。

UPDATE：

- WHERE 和赋值右侧都在旧行作用域求值；
- 同一行的全部新值先计算并验证，再替换编码行；
- 隐藏 ROWID 永不改变；
- 不满足 WHERE 的行不计入影响行数。

DELETE：

- WHERE 在目标表旧行上求值；
- 只删除满足条件的 ROWID 键；
- 已提交删除的 ROWID 不回收。

UPDATE/DELETE 不把全部目标行放入内存。执行器按“从上一个 ROWID 之后查找下一行、释放扫描借用、再修改当前键”的方式前进。因为 4C1 不允许修改隐藏 ROWID，这一策略不会重复处理、跳过或产生 Halloween 问题。每次重新定位可以是 O(log N)；索引和更高效的可变游标属于后续性能优化，不改变语义。

影响行数使用受检查的 u64 计数，溢出返回 `ResourceLimit`。

## 12. 事务与写者所有权

### 12.1 自动提交

- Connection 在 Idle 状态执行 SELECT 时创建一个由 Rows 持有的只读快照；
- Connection 在 Idle 状态执行 DDL/DML 时非阻塞地取得写者资格；
- 语句成功后提交完整 WriteBatch；
- 语句失败时丢弃该批次，不产生可见修改；
- 自动提交 DDL 与 DML 使用相同 WAL 耐久边界。

### 12.2 SQL 显式事务

Connection 保存以下会话状态：

```text
Idle
SqlWrite(Active WriteBatch)
SqlWriteFailed(WriteBatch retained only for rollback)
NeedsReopen
```

- BEGIN 只允许从 Idle 进入 SqlWrite；
- BEGIN 在只读数据库上返回 Unsupported；
- BEGIN 遇到已有写者时立即返回 Busy；
- 活动 SQL 写事务内的查询读取自己的未提交修改；
- COMMIT 只允许结束 SqlWrite；
- ROLLBACK 可以结束 SqlWrite 或 SqlWriteFailed；
- Idle 中的 COMMIT/ROLLBACK、重复 BEGIN 或任何嵌套尝试返回 Transaction；
- Connection 被丢弃时，活动批次只释放内存修改并回滚。
- 提交 I/O 失败消耗活动批次并进入 NeedsReopen；该 Connection 的后续 prepare/query/execute 和新事务请求均返回 Transaction，直到调用方关闭并重新打开数据库。

### 12.3 Rust 事务对象

现有 `ReadTransaction<'connection>` 与 `WriteTransaction<'connection>` 的可变借用合同继续阻止同一 Connection 重入。Connection 已有活动 SQL 事务时，`begin_read`/`begin_write` 返回 Transaction。Rust 写事务继续要求显式 `commit()`；Drop 或 `rollback()` 不发布修改。

### 12.4 自持有写租约

当前 WriteBatch 通过借用 Pager 中长期持有的 MutexGuard 获得写者资格，无法安全地存入拥有同一 Pager 的 Connection。4C1 将其替换为自持有的 `WriterLease`：

- WriterCoordinator 通过短期 Mutex 临界区维护是否已有活动写者；
- 非阻塞获取在已有活动写者时返回 Busy；
- WriterLease 自己拥有协调器的 Arc，不借用 Connection 或 Pager 字段；
- WriteBatch 自己拥有 Pager 句柄和 WriterLease，不再需要存储自引用生命周期；
- WriterLease Drop 清除活动状态；
- WriteBatch 的 dirty 页、候选目录和 WAL 提交算法保持不变；
- Checkpoint 与全部写批次必须使用同一个协调器，继续与写提交互斥；
- 公共 Rust 事务仍通过 PhantomData 或等价安全类型约束借用 Connection。

此调整只改变内存所有权和锁租约表达方式，不改变主文件、WAL 格式、提交 LSN、快照或恢复语义。现有并发、锁顺序、崩溃和 Drop 未完成批次测试必须继续通过。

## 13. 语句级原子性

WriteBatch 已有 mutation checkpoint/restore 能力。每条显式事务内的 DDL/DML 开始前创建语句检查点，覆盖 dirty 页、分配、释放、候选头、目录状态和被替换根等全部可变集合。

- 成功：丢弃检查点，保留本语句修改；
- Syntax、Bind、SchemaChanged、Type、Constraint、Busy、Unsupported 或 ResourceLimit：恢复检查点，事务保持可用；
- Io、Corruption 或 Internal：先尽最大能力恢复内存检查点，再进入 SqlWriteFailed 或等价失败状态，只允许回滚。

参数、模式和 DeferredUnsupported 检查发生在建立语句检查点及任何写入之前。自动提交写语句的整个 WriteBatch 本身就是语句边界，可以直接丢弃失败批次。

提交期间发生 I/O 错误时，Phase 3 的耐久 Commit Record 仍是重开后的唯一事实来源。当前 Connection 不得继续写入；调用方必须关闭并重新打开数据库确认恢复结果。

## 14. 模式身份与可见性

每次 query/execute 都必须调用 Phase 4B 已证明的模式验证入口，不能复制另一套判断。验证使用执行上下文的目录视图：

- 自动提交使用开始本语句时的最新视图；
- ReadTransaction 使用其固定快照；
- WriteTransaction 和 SQL 显式事务使用包含本事务修改的批次视图。

验证继续覆盖数据库 UUID、TableId 和 SchemaVersion。CREATE 计划另外验证名称仍为空闲；DROP 和 DML 使用绑定 ID，不按名称重新查找。

以下情况返回 SchemaChanged：

- Statement 来自另一个数据库；
- 依赖表已删除；
- 同名表被删除后重建；
- 依赖表的 schema version 改变；
- DROP 绑定对象不再是原对象。

无关表的变化不得使 Statement 失效。4C1 不自动重新 prepare，也不回退到字符串或第三方 AST。

## 15. 错误合同

保留现有稳定错误分类，并新增：

```text
ResourceLimit = 1650
```

执行期映射：

| 情况 | ErrorCode | 事务结果 |
|---|---|---|
| 参数缺失、多余、重复或样式错误 | Bind | 无修改 |
| 值类型不兼容、算术溢出、除零 | Type | 回滚本语句 |
| Statement 身份或依赖版本变化 | SchemaChanged | 无修改 |
| 查询工作内存或执行计数超限 | ResourceLimit | 回滚本语句 |
| 另一个写者已存在 | Busy | 不取得写者 |
| 非法事务状态或嵌套事务 | Transaction | 保留既有事务状态 |
| 4C2/Phase 5 功能 | Unsupported | 无修改 |
| 文件或系统调用失败 | Io | 显式事务只能回滚 |
| 持久格式或引用损坏 | Corruption | 显式事务只能回滚 |
| 内部不变量失败 | Internal | 显式事务只能回滚 |

表达式错误使用 Plan 中保存的 SqlLocation。纯存储、锁和恢复错误没有 SQL 位置。Phase 4C1 不附加新的 Oracle ORA 编号；兼容映射属于 Phase 5。

## 16. 测试策略

### 16.1 单元测试

- 每种计划节点的确定性转换和第三方类型隔离；
- 参数规范化、精确集合检查、Unknown 和 typed NULL；
- 每种表达式、三值 AND/OR、NULL 传播、LIKE 和安全数值提升；
- 无 FROM、INNER JOIN、LEFT JOIN 和多表嵌套循环；
- 空输入与分组聚合、HAVING、DISTINCT、排序方向和 NULL 位置；
- 工作内存精确边界、计数溢出和 Drop 中途 Rows；
- DeferredUnsupported 在写者获取与任何修改前失败；
- 规划和执行随机有界输入不 panic。

### 16.2 API 集成测试

- `CREATE -> INSERT -> SELECT -> UPDATE -> DELETE -> DROP` 完整闭环；
- 位置与命名参数、重复引用、缺失、多余和类型错误；
- Statement 克隆、跨连接复用和错误 API 调用；
- 流式多行读取、提前 Drop、结果 metadata 和影响行数；
- DDL 后同事务 prepare、读己之写和其他连接不可见；
- 自动提交重开可见，未提交 Drop 重开不可见。

### 16.3 事务、并发与模式测试

- SQL 与 Rust 事务的提交、回滚、Drop 和互斥；
- DDL 可回滚且不发生隐式提交；
- 普通失败只回滚当前语句，随后语句仍可提交；
- Io/Corruption/Internal 后只允许回滚；
- 多个固定读快照与一个写者并存；
- 第二写者立即 Busy；
- 长 Rows 生命周期不观察未来提交；
- 删除、重建、版本改变和跨数据库 Statement 均返回 SchemaChanged；
- 无关表变化不使 Statement 失效。

### 16.4 恢复与模型测试

现有存储故障工具通过真实 SQL DDL/DML 触发代表性 WAL 失败点，验证：

- 报告成功的自动提交和显式提交在重开及子进程终止后可恢复；
- 未提交 SQL 事务和失败语句不可见；
- 目录、next_rowid 与行树始终共同发布或共同回滚；
- 恢复后模式验证与查询结果一致。

建立独立的小型内存参考模型，对固定种子的小表、NULL、CRUD、JOIN、聚合、排序和事务序列比较结果。SQLite 可以用于人工交叉检查公共子集，但不能作为唯一判定标准，因为其动态类型、NULL 和事务边界与本项目并不完全相同。

### 16.5 规模烟测与 CI

保留现有 Linux stable、Rust 1.85 MSRV、Windows、存储模型、WAL 恢复和目录恢复任务。新增独立 Linux Phase 4C1 execution job，最长 60 分钟：

- 在一个显式事务中重复执行已准备 INSERT，写入约 10 万行；
- 执行全表过滤、聚合、排序、UPDATE 和 DELETE；
- 验证结果、影响行数、内存预算和重开后的最终状态；
- 不设置依赖共享 CI 主机速度的硬毫秒阈值，只要求在任务时限内正确完成。

1000 万行/10 GB 数据集在 4C2 索引完成后作为独立专项测试，不进入 4C1 每次提交门槛。

## 17. Phase 4C1 退出条件

进入 Phase 4C2 前必须全部满足：

- 无约束表可以通过公共 SQL API 完成 DDL 和 CRUD；
- Phase 4B 接受的完整 SELECT 范围得到真实、可重复且受限的执行；
- Statement 在每次执行前复用既有身份与模式验证；
- Rows 流式接口、参数和影响行数公共合同稳定；
- 自动提交、SQL 事务和 Rust 事务均满足批准语义；
- 普通失败只回滚当前语句，致命失败不能继续写；
- DDL、目录、next_rowid 和用户行只通过正确 WAL 边界发布；
- WriterLease 调整不改变磁盘格式或破坏现有并发与恢复证明；
- 索引和约束不能被静默接受或部分执行；
- 全部现有测试、新增测试、MSRV 和 Windows CI 通过；
- 10 万行 Linux 烟测在独立任务时限内通过；
- 源码不存在成本优化器、临时磁盘执行、4C2 维护或 Phase 5 Oracle 行为。

## 18. Phase 4C2 交接

Phase 4C2 可以依赖 4C1 的稳定边界：

- Plan 与拉取式算子可以加入 IndexScan，而无需改变 SQL 前端；
- DML sink 已有语句检查点，可以把表与索引修改放入同一原子语句；
- WriteBatch 与 WriterLease 可以共同承载表、目录、索引和约束修改；
- 参数、模式验证、Rows、ExecutionResult 和事务 API 不需要为索引重写；
- 全表扫描仍作为无索引和优化器拒绝索引时的正确性基线。

Phase 4C2 必须新增索引目录格式、索引键编码、唯一性与引用约束维护，并证明表树和全部相关索引在同一 WAL 提交中共同发布。它不得以修改 4C1 公共语义为代价实现索引。
