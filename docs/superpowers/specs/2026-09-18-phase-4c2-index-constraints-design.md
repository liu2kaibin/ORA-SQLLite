# ORA-SQLLite Phase 4C2 索引与约束设计

日期：2026-09-18

状态：已完成对话设计，等待书面规格复核

基础版本：`3a8e75a47565bc4d774aa4a9c08657c4a2807ea7`

## 1. 目标

Phase 4C2 在 Phase 4C1 已证明的公共 SQL 执行、关系存储、语句检查点、WAL、快照和单写者边界上，加入持久化二级索引和首版即时约束，使首版关系 CRUD 具备以下能力：

- `CREATE [UNIQUE] INDEX` 与 `DROP INDEX`；
- `PRIMARY KEY`、`UNIQUE`、`NOT NULL`、`CHECK` 和即时 `FOREIGN KEY`；
- INSERT、UPDATE、DELETE 对表和全部相关索引的原子维护；
- 单表 SELECT 的确定性 `IndexScan`；
- 表、目录、索引和约束通过同一 WAL 提交共同发布或共同回滚；
- format 2 数据库向前兼容，不执行启动迁移或全库重写。

Phase 4C2 不改变 Phase 4C1 的公共 `Connection`、`Statement`、`Rows`、`ExecutionResult` 和事务合同。

## 2. 已批准的设计决定

1. 保持主文件 format 2，通过单调特性位惰性启用索引和约束目录。
2. `PRIMARY KEY` 和 `UNIQUE` 自动创建系统管理的唯一索引。
3. 主键禁止 NULL；普通唯一键允许多个包含 NULL 的键。索引仍记录全部 NULL 键。
4. 外键子列不自动创建索引。有匹配索引时使用，否则扫描子表。
5. 被外键引用的父表不能直接删除；本阶段不支持 `CASCADE CONSTRAINTS`。
6. 规则规划器只使用前导等值列和其后一列的范围，不优化 JOIN、排序或分组。
7. Phase 4C2 以一个统一规格约束四个独立验收和合并阶段。

## 3. 非目标

以下能力不进入 Phase 4C2：

- `ALTER TABLE`、`ADD/DROP CONSTRAINT`；
- 自引用或循环外键；
- 延迟约束；
- `ON DELETE/UPDATE CASCADE`、`SET NULL` 或 `SET DEFAULT`；
- 以普通显式唯一索引代替父键约束；
- JOIN、ORDER BY、GROUP BY、DISTINCT 或 DML 目标扫描的索引优化；
- 覆盖索引、函数索引、部分索引、降序索引、在线建索引；
- 统计信息、成本优化器、并行执行或磁盘临时执行；
- Phase 5 的 Oracle 类型声明、函数、序列、`DUAL`、`ROWNUM`、分页和空字符串兼容；
- 每次打开数据库时全表重建或比较全部索引；
- 修改 B+Tree、WAL 或主文件页的既有基础格式。

## 4. 分层架构

Phase 4C2 沿用现有单向依赖：

```text
SQL parser / binder
        |
        v
immutable Plan + deterministic index choice
        |
        v
query operators / mutation sink
        |
        v
ConstraintCatalog + IndexStorage + Relation
        |
        v
CatalogReader/Writer + B+Tree + WriteBatch
        |
        v
WAL commit / snapshot / checkpoint / recovery
```

新增内部边界：

- `IndexStorage`：负责索引键编解码、索引树读写、唯一性前缀检查和 ROWID 输出；
- `ConstraintCatalog`：提供表约束、被引用关系和系统索引所有权的只读视图；
- `ConstrainedTableWrite`：组合现有 `TableWrite`、相关索引和约束检查。SQL 执行器不得直接写目录字节或索引 B+Tree；
- `IndexScan`：从索引范围取得 ROWID，再从表树读取完整行。

现有 `WriteBatch` 是唯一持久化修改边界。Phase 4C2 不引入第二套日志、影子数据库、临时数据库或后台索引构建器。

## 5. format 2 与特性位

新增：

```rust
const CATALOG_V1: u64 = 1 << 0;
const INDEX_CONSTRAINT_V1: u64 = 1 << 1;
```

兼容规则：

- 没有 `INDEX_CONSTRAINT_V1` 的 format 2 数据库继续按 Phase 4C1 读取和写入；
- 首次成功创建显式索引或受约束表时，在同一个 WriteBatch 中写入全部新目录记录并启用该位；
- 特性位一旦提交不得清除，即使所有索引和约束后来都被删除；
- 特性位存在但没有索引或约束记录是合法的空 4C2 状态；
- 4C2 记录存在但特性位缺失属于 `Corruption`；
- `INDEX_CONSTRAINT_V1` 存在但 `CATALOG_V1` 缺失属于 `Corruption`；
- 不认识该位的旧程序必须在写入、恢复或截断前返回 `Unsupported`；
- 只读打开不得启用特性位、补写目录记录或改变文件；
- 启动时不执行迁移、索引重建或全库页面重写。

WAL CommitState 已持久化 `feature_bits`，因此新位沿用现有单调校验、快照可见性、Checkpoint 和恢复规则。

## 6. 目录键空间

现有 `0x00` 和 `0x10..=0x14` 保持不变。新增的键族均使用大端 ID，使同一所有者的记录连续排序。

### 6.1 索引记录族

| 族 | 名称 | 键负载 | 值 |
|---|---|---|---|
| `0x20` | IndexName | 名称长度 + UTF-8 名称 | IndexId 映射 |
| `0x21` | IndexDefinition | IndexId | IndexDef V1 |
| `0x22` | TableIndex | TableId + IndexId | IndexId 映射 |
| `0x23` | IndexColumn | IndexId + `u16` 顺序 | ColumnId 映射 |

### 6.2 约束记录族

| 族 | 名称 | 键负载 | 值 |
|---|---|---|---|
| `0x30` | ConstraintName | 名称长度 + UTF-8 名称 | ConstraintId 映射 |
| `0x31` | ConstraintDefinition | ConstraintId | ConstraintDef V1 |
| `0x32` | TableConstraint | TableId + ConstraintId | ConstraintId 映射 |
| `0x33` | ConstraintColumn | ConstraintId + `u16` 顺序 | ColumnId 映射 |
| `0x34` | ReferencedBy | 父 ConstraintId + 子 ConstraintId | 子 ConstraintId 映射 |

所有名称继续遵守 1–128 UTF-8 字节的 SQL 标识符限制。索引名称在索引命名空间中全库唯一；约束名称在约束命名空间中全库唯一。

未命名约束首先尝试生成 `SYS_C<object_id>`。如果该名称已被显式约束占用，则依次尝试 `SYS_C<object_id>_1`、`SYS_C<object_id>_2`，直到找到空闲名称；后缀增长和 128 字节限制使用检查算术。生成名称与显式名称使用同一唯一性规则，成功后持久化且不再重新计算。索引和约束命名空间分离，因此系统索引可以与其拥有约束使用同一名称。

## 7. 目录记录

全部新记录沿用现有目录信封：记录族、记录版本 V1、零保留位、检查长度和严格 EOF。

### 7.1 IndexDef V1

IndexDef 至少保存：

- IndexId；
- TableId；
- 当前索引根 `PageRef`；
- 列数量；
- `unique` 标志；
- 所有权：`Explicit` 或 `Constraint(ConstraintId)`；
- 持久化名称。

显式索引的 ConstraintId 必须为零。系统索引必须引用同一表上的 `PRIMARY KEY` 或 `UNIQUE` 约束，并由该约束反向引用。

IndexColumn 从 0 开始连续编号，不得重复、缺失或引用其他表的列。索引至少包含一列，最多包含表的全部列。

### 7.2 ConstraintDef V1

ConstraintDef 至少保存：

- ConstraintId；
- 所属 TableId；
- 类型：PrimaryKey、Unique、NotNull、Check 或 ForeignKey；
- 本地列数量；
- 可选的系统 IndexId；
- ForeignKey 的父 TableId 和父 ConstraintId；
- CheckProgram V1 字节及长度；
- 持久化名称；
- 为将来动作和延迟模式保留且必须为零的字段。

类型不使用的字段必须为零。解码器不得忽略互相矛盾的标志或引用。

约束交叉规则：

- 一张表最多一个主键；
- 主键、唯一约束必须恰好引用一个系统唯一索引；
- NotNull 必须恰好引用一个本地列且没有索引；
- Check 没有 ConstraintColumn 记录，其列引用包含在 CheckProgram 中；
- ForeignKey 的本地列与父约束列数量相同，顺序有意义；
- ReferencedBy 记录必须与每个 ForeignKey 一一对应；
- 父 ConstraintId 必须属于父 TableId，且类型为 PrimaryKey 或 Unique。

目录候选验证必须检查正向、反向、计数、所有权、名称和表归属，不接受孤儿、重复或仅单向存在的记录。

## 8. 索引键 V1

每个物理索引键为：

```text
component-0 || component-1 || ... || component-N || rowid-be
```

列类型来自 IndexColumn 对应的 ColumnDef，因此键内不重复保存类型标签。每个分量以 NULL 标志开始：

- `0x00`：非 NULL，后接类型负载；
- `0x01`：NULL，无负载。

此标志使升序物理键中非 NULL 位于 NULL 之前。Phase 4C2 不使用索引顺序消除 ORDER BY，因此 SQL 的 NULL 排序仍由现有 Sort 算子决定。

非 NULL 负载：

- Int64：大端补码后翻转最高符号位；
- Decimal：先将零统一为零编码；非零 magnitude 反复移除十进制尾零，并同步减少 scale。令 `digits` 为规范 magnitude 的十进制数字，`adjusted_exponent = digits.len() - scale`。正数尾部编码为翻转符号位的 `i16` 大端 adjusted exponent、每个十进制数字加一后的字节及 `0x00` 终止符；负数使用正数尾部的逐字节反码。完整前缀依次为负数 `0x00`、零 `0x01`、正数 `0x02`。adjusted exponent 必须在 Decimal 合法输入可推导的 `-37..=76` 范围内。该规范使数值相等但 scale 不同的 Decimal 得到同一键，并使字节顺序等于 `sql_total_cmp`；
- Float64：先拒绝非有限值并把正负零规范为正零；负数位模式按位取反，非负数翻转符号位；
- Text：有效 UTF-8 字节序。UTF-8 字典序与 Unicode 标量顺序一致；
- Blob：原始字节序；
- Date/Timestamp/Bool：按现有字段比较顺序使用固定宽度大端编码；
- 变长 Text/Blob 中的 `0x00` 编码为 `0x00 0xff`，分量结束编码为 `0x00 0x00`。

ROWID 始终以 8 字节大端追加。索引叶值为空；IndexScan 从键尾取得 ROWID。

全部索引记录所有行，包括部分或全部列为 NULL 的行。唯一性不是依赖 B+Tree 物理键重复，而是在插入前扫描不含 ROWID 的逻辑键前缀：

- PrimaryKey 的任何 NULL 在编码前失败；
- Unique 或显式唯一索引只在全部索引列非 NULL 时检查相同前缀；
- 任一列为 NULL 时跳过唯一性检查，但仍插入带 ROWID 的物理键；
- 唯一性检查遇到相同 ROWID 是当前 UPDATE 的旧条目，不算冲突；其他 ROWID 返回 `Constraint`。

完整物理键不得超过现有 B+Tree 1024 字节上限。SQL 层在任何表或索引修改前编码全部相关键；超限返回 `ResourceLimit`。CREATE INDEX 扫描旧行时遇到超限键，恢复整个语句。

Blob 只允许等值索引条件。范围规划不得为 Blob 生成上下界。

索引键解码器必须检查：分量数量、标志、转义终止、固定宽度、有限 Float、Decimal 规范形、ROWID 非零及严格 EOF。

## 9. 索引生命周期

### 9.1 CREATE INDEX

执行顺序：

1. 验证数据库身份、表模式版本、名称仍为空闲及全部参数合同；
2. 获取写者并建立语句检查点；
3. 分配 IndexId 和空 Index B+Tree 根；
4. 按 ROWID 流式扫描表；
5. 对每行编码键、执行唯一性检查并插入索引；
6. 写入 IndexName、IndexDefinition、TableIndex 和连续 IndexColumn；
7. 递增表模式版本；
8. 首次需要时启用 `INDEX_CONSTRAINT_V1`；
9. 完成 CatalogWriter 并通过现有 WAL 提交。

创建过程不把全部行或键放入集合，但同一 WriteBatch 仍持有提交所需的脏页。Phase 4C2 不引入外部排序、临时文件或多提交在线构建。

任何重复、键超限、解码、I/O 或资源错误恢复语句检查点。显式事务中的普通错误不消耗事务。

### 9.2 DROP INDEX

- 重新验证 IndexId、所属表模式版本和显式所有权；
- 系统约束索引返回 `Constraint`；
- 删除全部索引目录记录并释放整棵索引树；
- 递增所属表模式版本；
- 全部操作在一个语句检查点内完成。

### 9.3 DROP TABLE

- 存在 ReferencedBy 记录时在释放任何页面前返回 `Constraint`；
- 原子删除该表拥有的显式索引、系统索引、约束、对外 ForeignKey 及其 ReferencedBy 记录；
- 释放全部索引树和表树；
- 删除现有表、列及新增目录记录；
- 不清除 `INDEX_CONSTRAINT_V1`。

## 10. 约束语义

### 10.1 PrimaryKey 与 Unique

- PrimaryKey 隐含相关列不可空，并拥有系统唯一索引；
- Unique 拥有系统唯一索引并使用第 8 节的 NULL 语义；
- 同一列上的显式 NotNull 与 PrimaryKey 可以共存，各自保留稳定约束名称；
- 系统索引生命周期完全由约束和表控制。

### 10.2 NotNull

NotNull 同时写入 ConstraintDef，并使对应 ColumnDef 的 `nullable` 为 false。现有 RowCodec 是最后的物理行保护，SQL 执行器在编码行和索引键前返回带约束名称的 `Constraint`。

### 10.3 CheckProgram V1

CHECK 表达式编译为持久、类型化、后序指令流。指令只允许：

- 本表稳定 ColumnId；
- Phase 4C1 可持久化的标量常量；
- 当前 BoundExpr 支持的算术、布尔、比较、IS NULL、IN、BETWEEN 和 LIKE；
- 显式操作数数量和结果 SqlType。

禁止参数、聚合、其他表列以及 Phase 5 函数。CHECK 程序不保存 SQL 文本、解析器节点、源码跨度或进程内枚举布局。操作码和标量编码必须有独立的 V1 稳定磁盘定义。

程序最多 1 MiB，并同时受创建时 SqlLimits 的节点和深度限制。验证使用显式栈，不依赖递归。DML 执行时，解码栈、临时值和表达式结果全部计入现有 WorkMemory；预算不足返回 `ResourceLimit`，不得绕过限制或缓存一份不计费的完整程序。程序必须恰好产生一个 Bool/NULL 结果：

- FALSE：返回 `Constraint`；
- TRUE 或 NULL：通过；
- 类型、数值或资源错误按原始错误分类返回，并恢复当前语句。

### 10.4 ForeignKey

- 父列必须按相同顺序完整匹配一个已声明的 PrimaryKey 或 Unique；
- 不能仅依赖显式唯一索引；
- 本地和父列数量一致；
- 列类型必须相同；唯一允许的差异是现有无损 Int64 到 Decimal 统一，本地值按父键类型编码；
- 本地任一列为 NULL 时检查通过；
- 非 NULL 子键必须在当前 WriteBatch 可见的父系统索引中存在；
- 父键 UPDATE/DELETE 必须确认当前 WriteBatch 可见的子行中不存在旧键；
- 有完全相同前导列的子索引时使用该索引，否则按 ROWID 扫描子表；
- 不支持自引用、循环、延迟和引用动作。

由于本阶段没有 ALTER TABLE，约束只随 CREATE TABLE 建立；父表必须已经存在，自引用因而在绑定阶段返回 `Unsupported`。

## 11. DDL 与模式版本

受约束 CREATE TABLE：

1. 在任何目录写入前验证全部名称、列、约束、父约束和 CheckProgram；
2. 预先分配表、列、约束和系统索引的稳定对象 ID；
3. 创建空表根和空系统索引根；
4. 写入完整双向目录记录；
5. 启用特性位并完成 CatalogWriter；
6. 通过同一 WAL 提交发布。

受约束表创建失败不得消耗可见对象、页面或对象 ID。

CREATE/DROP INDEX 和受约束表 DDL 递增相关表的 SchemaVersion。普通 DML 即使导致表根或索引根分裂，也不改变 SchemaVersion。

Prepared Statement 仍先验证数据库 UUID 和全部表 SchemaVersion。计划只保存 IndexId，不保存 PageRef；执行时从当前事务快照目录取得索引根。因此普通 DML 后无需重新 prepare，而索引 DDL 后旧计划稳定返回 `SchemaChanged`。

## 12. DML 原子维护

每条显式事务 DDL/DML 在现有外层 StatementCheckpoint 内执行。检查点覆盖表页、索引页、分配、释放、目录状态、表根、索引根、ROWID 和特性位。

### 12.1 INSERT

1. 求值并构造完整行；
2. 检查 NotNull、RowCodec、CheckProgram；
3. 编码全部索引键并检查 1024 字节上限；
4. 检查全部唯一键和 ForeignKey；
5. 插入表行和全部索引条目；
6. 更新发生分裂的表与索引根；
7. 完成目录并返回影响行数。

### 12.2 UPDATE

继续按“寻找上一个 ROWID 之后的下一行”流式推进：

1. 保存旧行和旧索引键；
2. 使用同一个旧行求值全部赋值；
3. 构造完整新行并执行全部预检查；
4. 唯一性检查排除当前 ROWID 的旧条目，但包含事务内已修改行和尚未处理的旧行；
5. 检查子 ForeignKey 及被修改父键的引用；
6. 删除旧索引项，替换表行，插入新索引项；
7. 释放当前行内存后继续下一个 ROWID。

即时唯一性允许语句因尚未处理行的旧键而失败；Phase 4C2 不增加延迟唯一约束或键交换缓冲。

### 12.3 DELETE

对每个匹配 ROWID：

1. 编码旧索引键；
2. 检查该行的父键是否仍被引用；
3. 删除全部索引项和表行；
4. 更新根并继续下一个 ROWID。

后续行发生普通错误时，外层检查点恢复此前所有行和索引修改。

## 13. IndexScan 规则

Phase 4C2 只从单表 SELECT 的 WHERE 中提取由 AND 连接的可搜索条件。

可用条件：

- 前导列连续 `column = constant/parameter`；
- 前导列 `IS NULL`；
- 等值前缀之后同一下一列的一个下界和一个上界；
- `BETWEEN` 可规范化为包含上下界；
- `<`、`<=`、`>`、`>=` 保留包含性；
- Blob 仅允许等值条件。

不可用条件留给完整 Filter：OR、NOT、`!=`、列间比较、函数、算术结果、IN 多范围、LIKE 前缀、JOIN 条件及非前导列条件。

确定性选择顺序：

1. 完整且非 NULL 的唯一键等值；
2. 等值前缀列数量最多；
3. 有下一列范围边界；
4. 边界数量更多；
5. IndexId 更小。

不读取统计信息，不估算成本。没有候选时使用 TableScan。

IndexScan 根据执行参数生成字节范围，逐个取得 ROWID、读取表行，并重新执行完整 WHERE。索引条目缺少对应表行属于 `Corruption`；Filter 不得掩盖该错误。

Phase 4C2 的 UPDATE/DELETE 仍使用 ROWID TableScan，以保持 Phase 4C1 已证明的 Halloween 安全性。索引仅用于唯一性和外键检查。

## 14. 快照、恢复与验证

- 读事务从自己的 CatalogReader 取得 IndexDef 和根；
- 旧快照继续看到旧表根、旧索引根和旧约束集合；
- 新快照只能看到完整 WAL CommitState 发布后的表与索引；
- Checkpoint 从目标 LSN 的完整状态写回，不把更新索引根与旧表根混合；
- WAL 恢复必须验证特性位单调、目录交叉引用、Index 根 PageRef 和 Index TreeKind；
- 表页、目录页、Freelist 页及每棵索引树的可达页面必须互不重叠；
- 索引结构验证解码全部键并检查严格排序、列数、类型和 ROWID；
- 正常打开不逐行比较表和索引，以避免 O(database) 启动成本；
- 模型测试、故障注入和专项验证负责证明逻辑条目一致性。

索引或约束候选验证失败时不得发布一个更旧的完整提交来掩盖损坏；只有 Phase 3 已证明的未完成 WAL 尾部可以被丢弃。

## 15. 错误与资源边界

| 情况 | ErrorCode | 事务效果 |
|---|---|---|
| 主键、唯一、非空、CHECK、外键违反 | Constraint | 恢复语句，显式事务可继续 |
| 删除系统索引或被引用父表 | Constraint | 无修改 |
| 索引键或 CheckProgram 超限 | ResourceLimit | 恢复语句 |
| 数据库身份或 SchemaVersion 变化 | SchemaChanged | 无修改 |
| 4C2 之外的约束选项 | Unsupported | 写者与修改前失败 |
| 参数、列或类型不匹配 | Type/Bind | 修改前失败 |
| 目录交叉引用、索引键或索引页损坏 | Corruption | 沿用致命事务规则 |
| I/O 或 Internal | Io/Internal | 沿用 Phase 4C1 rollback-only/NeedsReopen 规则 |

Constraint 错误包含稳定约束名称。显式唯一索引冲突包含稳定索引名称。Phase 4C2 不新增 Oracle ORA 编号；编号映射仍属于 Phase 5。

所有计数、长度、偏移、对象 ID、ROWID、序号和内存估算使用检查算术。任何需要的键和约束程序必须在修改相应持久状态前验证。

## 16. 测试设计

### 16.1 单元测试

- 每个新目录键和值的手算黄金字节；
- 每个字节截断、错误版本、保留位、计数和交叉引用；
- 每种值的索引顺序、规范编码、NULL、正负零和边界；
- Text/Blob 转义、复合键边界和 1024 字节上下界；
- CheckProgram 操作码、类型栈、深度、节点和 1 MiB 上限；
- 系统索引所有权、稳定名称和连续目录映射。

### 16.2 模型与差分测试

- 普通、唯一、复合、NULL 索引结果与 TableScan 完全一致；
- 固定种子程序比较表行、索引条目、约束结果、影响行数和事务状态；
- 每次 INSERT/UPDATE/DELETE 后，模型证明每个可见行恰好拥有每个索引的一项；
- IndexScan 与完整 WHERE 的结果、顺序无关集合和错误分类一致；
- 子表有索引和无索引时 ForeignKey 结果一致。

### 16.3 故障、恢复与并发

- 覆盖目录记录、索引页帧、表页帧、Commit、WAL sync、内存发布和目录 sync；
- 每个中止点重新打开后只能看到完整旧状态或完整新状态；
- 索引根分裂、表根分裂、多个索引同时变化和约束目录共同进入矩阵；
- 长读快照不观察未来索引或部分提交；
- 单写者 Busy、Drop、rollback-only 和 Checkpoint 锁序保持不变；
- format 2 旧文件、首次特性启用、只读打开和未知特性拒绝均有真实文件测试。

### 16.4 CI 与规模

保留现有 Linux stable、Rust 1.85 MSRV、Windows、存储模型、WAL、目录和 Phase 4C1 任务。新增独立 Linux Phase 4C2 任务及专项崩溃矩阵。

常规 Phase 4C2 门槛：

- 10 万行普通、唯一和复合索引创建；
- 点查、范围查及需要同步维护索引的 DML；DML 目标定位仍使用 TableScan；
- 全套约束、回滚、重开和快照测试；
- 任务只要求在 CI 时限内正确完成，不使用共享 runner 硬毫秒阈值。

2026-10-04 的 MVP 裁定将后续规模验收上限固定为 10 万行。既有 1000 万行/10 GB 工具仅保留为可选诊断资产，不再执行，也不再阻塞 Phase 5。裁定前的部分规模运行已提交 470 万行并产生 18.286 GiB 临时数据库与 WAL；运行期间没有数据库断言、panic 或容量错误，随后因验收裁定变更而人工停止。该结果是 MVP 的部分规模证据，不表述为完整 1000 万行通过。

## 17. 分阶段交付

### 17.1 Phase 4C2-A：索引存储与 DDL

- 特性位与全部目录键空间；
- IndexDef、IndexColumn 和目录验证；
- 索引键 V1；
- CREATE/DROP INDEX；
- INSERT/UPDATE/DELETE 的显式索引维护；
- 普通/唯一索引模型和恢复测试。

### 17.2 Phase 4C2-B：本表约束

- PrimaryKey、Unique、NotNull、Check；
- 系统索引所有权；
- CheckProgram V1；
- 受约束 CREATE TABLE；
- 稳定名称和 Constraint 错误。

### 17.3 Phase 4C2-C：外键

- ForeignKey 与 ReferencedBy 目录；
- 子写和父 UPDATE/DELETE 检查；
- 可选子索引与全表扫描；
- DROP TABLE 依赖拒绝；
- 跨表快照、恢复和故障矩阵。

### 17.4 Phase 4C2-D：IndexScan 与规模验收

- 确定性可搜索条件提取；
- IndexScan 和完整 Filter 重检；
- 索引/扫描差分；
- 10 万行常规 CI；
- 完整 4C2 故障矩阵；
- 10 万行规模验收结果；可选大规模工具不作为交付门槛。

每个阶段独立评审、测试和合并。后一阶段只能依赖已经进入 main 的前一阶段，不在一个 PR 中跨越多个合并门。

## 18. Phase 4C2 退出条件

进入 Phase 5 前必须全部满足：

- 显式普通和唯一索引可创建、维护、删除、重开和恢复；
- PrimaryKey、Unique、NotNull、Check 和即时 ForeignKey 无漏检；
- 表与全部相关索引始终共同提交或共同回滚；
- IndexScan 与 TableScan 对所有支持计划得到相同结果和错误；
- 旧 format 2 数据库无需迁移即可继续使用；
- 新特性位、目录和旧程序拒绝路径全部通过真实文件测试；
- 目录、索引、约束、表根、Freelist 和 WAL 的损坏均被控制地检测；
- 多读单写、长快照、Checkpoint 和恢复证明没有回退；
- Linux stable、Rust 1.85 MSRV、Windows 和全部专项 CI 通过；
- 10 万行常规门槛通过；
- 按 2026-10-04 MVP 裁定，规模验收最多运行 10 万行；1000 万行/10 GB 不再是 Phase 5 阻塞项；
- 源码不存在 ALTER、级联/延迟外键、成本优化器、索引 JOIN、临时磁盘执行或 Phase 5 行为。

## 19. Phase 5 交接

Phase 5 可以依赖以下稳定边界：

- 索引键 V1 已为当前核心值定义确定顺序；
- 约束和系统索引拥有稳定目录身份与错误名称；
- CheckProgram 可以版本化扩展经批准的 Oracle 函数操作码；
- 规则规划器可以为 Phase 5 分页和 ROWNUM 使用既有扫描输出，而无需修改存储协议；
- 序列必须使用独立对象记录和耐久分配区间，不得复用事务性索引计数器；
- Oracle 类型声明映射若改变持久 ColumnType 或索引排序，必须新增格式能力，不得静默改变 IndexKey V1。
