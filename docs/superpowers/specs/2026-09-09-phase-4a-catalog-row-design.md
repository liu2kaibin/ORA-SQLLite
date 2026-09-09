# ORA-SQLLite Phase 4A 系统目录与行编码设计

日期：2026-09-09

状态：交互设计已逐节确认，等待书面规格审核

## 1. 目标与阶段位置

Phase 4 按三个可独立验收的子阶段推进：

1. Phase 4A：系统目录、对象身份、隐藏 ROWID 与用户行编码。
2. Phase 4B：SQL 词法/语法、名称与参数绑定，以及公共数据库 API。
3. Phase 4C：DDL、基础 CRUD、索引维护与约束执行。

本规格只覆盖 Phase 4A。其目标是在 Phase 3 已证明的 Pager、B+Tree、WAL、快照和单写者事务之上建立稳定的关系元数据与行存储边界，使后续 SQL 层无需了解页格式或 WAL 细节。

Phase 4A 完成后，内部代码能够原子地初始化目录、持久化表和列定义、分配稳定对象 ID 与 ROWID，并按照表模式编码和解码用户行。它仍不提供可供应用调用的 SQL 或 CRUD 接口。

## 2. 范围

### 2.1 本阶段实现

- 一个由现有 B+Tree 承载的持久化系统目录。
- `CatalogMeta`、`TableDef` 和 `ColumnDef` 的稳定磁盘格式。
- `ObjectId`、`TableId`、`ColumnId`、`IndexId`、`ConstraintId`、`SchemaVersion` 和 `RowId` 等内部强类型。
- 表名、列名、对象定义和从属顺序所需的目录键空间。
- 空目录的可写初始化、只读空视图、重新打开和完整一致性验证。
- 全局对象 ID 和逐表隐藏 ROWID 的事务内分配。
- 基于列顺序、NULL 位图和偏移目录的用户行格式。
- 所有现有 `Value` 变体的行值编码和解码。
- 目录根、目录记录和用户数据树能够在同一 `WriteBatch` 中原子发布的内部接口。
- 磁盘损坏、边界、回滚、恢复、并发快照和故障注入测试。

### 2.2 明确不实现

- SQL 词法、语法、AST、Oracle 改写、名称绑定或参数绑定。
- 公共 `Database`、`Connection`、`Statement`、`Transaction` 或 CRUD API。
- DDL/DML 执行器、查询计划和行流算子。
- 索引创建、索引维护、索引扫描或唯一性检查。
- 主键、唯一、非空、检查或外键约束执行。
- 默认表达式、生成列或存储表达式 IR。
- Oracle 类型名、精度/标度/字符长度声明和隐式转换。
- 空字符串转 NULL；Phase 4A 必须保留空 Text、空 Blob 与 NULL 的区别。
- 函数、序列、`DUAL`、`ROWNUM`、分页和其他 Phase 5 兼容行为。
- `ALTER TABLE`、在线模式迁移、旧行升级或跨进程并发。

## 3. 设计选择

### 3.1 采用单一引导目录 B+Tree

系统目录使用数据库头中的单一 `catalog_root`，不为每一类目录对象建立独立 B+Tree。键首字节划分记录族；固定宽度 ID 使用大端编码，使字节排序同时支持精确查找和按表前缀枚举。

目录树复用现有 `TreeKind::Table` 的叶和内部页格式。`catalog_root` 已提供目录语义，Phase 4A 不为目录另造一套 B+Tree 页面或修改已证明的树算法；现有单页 `PageType::Catalog` 仍保持保留状态，不被混入普通 B+Tree。

不采用以下方案：

- 自举 SQL 目录表：在 SQL 执行器和目录均不存在时形成启动循环。
- 每类对象一个目录树：增加根管理、检查点和原子发布复杂度。
- 独立 manifest 或追加日志：会在 WAL 之外形成第二套事务和恢复协议。
- 通用序列化 crate：无法保证长期稳定的逐字节格式及受控的未知字段策略。

### 3.2 稳定逻辑身份与物理根分离

每个目录对象拥有非零 64 位逻辑 ID。名称只负责查找，B+Tree 根只负责物理访问；目录内部引用一律使用逻辑 ID。表数据根使用完整 `PageRef`，同时保存页号与代次，避免页面回收后错误指向新对象。

对象 ID 全局单调递增。已提交后删除的 ID 永不复用；事务回滚未发布的分配不消耗持久化计数器。达到 `u64::MAX` 时返回受控资源/类型错误，不发生回绕。

### 3.3 模式有序、非自描述行

行按照 `TableDef` 的列序存储，不重复保存列名或类型。每行保存行格式版本、表模式版本、列数、NULL 位图、偏移目录和连续载荷。这比逐列保存名称和类型更小，也允许未来通过模式版本实现显式升级。

## 4. 内部模块与边界

Phase 4A 新增以下 crate 内部模块：

```text
src/storage/catalog/
  mod.rs       目录读取、写入和一致性边界
  key.rs       目录键族及顺序保持编码
  record.rs    CatalogMeta/TableDef/ColumnDef 磁盘格式
  types.rs     稳定 ID、名称、内部列类型与定义对象
src/storage/row.rs
  行头、NULL 位图、偏移目录和值编解码
```

所有新类型保持 `pub(crate)` 或更窄可见性。SQL AST、第三方解析器类型和未来公共 API 类型不得进入这些模块。

主要职责：

- `CatalogReader`：从一个 `ReadSnapshot` 或只读 `PageSource` 打开并验证目录，按名称/ID查找表和列，按列序枚举。
- `CatalogWriter`：持有当前目录 B+Tree 身份，显式接收 `&mut WriteBatch` 完成对象分配和记录变更。
- `RowCodec`：接收已经绑定的 `TableDef`/有序列定义、`Value` 列表和资源上限，输出或解析行字节。

目录对象不长期借用 `WriteBatch`。其变更方法接收批次参数，遵循现有 B+Tree API 的借用模型。每次可能改变目录根的操作后，目录对象保存新根；提交前调用 `finish`，通过 `track_tree` 与 `set_catalog_root` 登记当前根。遗漏 `finish` 必须使批次验证失败，而不是提交旧根。

## 5. 强类型与内存模型

以下 ID 都是非零 `u64` 新类型，不允许相互隐式转换：

- `ObjectId`
- `TableId`
- `ColumnId`
- `IndexId`
- `ConstraintId`
- `SchemaVersion`
- `RowId`

`TableId`、`ColumnId` 等可显式转换为 `ObjectId`，但反向转换必须由已解码的对象种类证明。

`CatalogName` 保存已经规范化的 UTF-8 名称。Phase 4A 只验证名称非空、编码合法且能够放入 1024 字节 B+Tree 键；Oracle 未加引号转大写、加引号保留大小写以及正式标识符长度限制由 Phase 4B 负责。目录只按传入字节做区分，不自行改变大小写。

`ColumnType` 在本阶段只描述已有内部值种类：

- `Int64`
- `Decimal`
- `Float64`
- `Text`
- `Blob`
- `Date`
- `Timestamp`
- `TimestampWithTimeZone`
- `Bool`

NULL 不是列类型。`ColumnDef.nullable` 描述物理行是否允许 NULL，但 Phase 4A 不执行 SQL 约束；`RowCodec` 仍会拒绝类型不匹配和对非空列传入 NULL，以保证持久化行符合已绑定模式。Oracle `NUMBER(p,s)`、`VARCHAR2(n)` 等声明参数在 Phase 5 的类型层加入新的目录记录版本。

单表最多 1000 列。列序号从 0 开始、连续且在同一表内唯一。模式版本从 1 开始，每次未来模式变更只能递增。

## 6. 目录特性位与初始化

Phase 4A 定义共享特性位：

```text
CATALOG_V1 = 1 << 0
```

数据库头和 WAL 提交记录必须引用同一份已知特性位常量，避免两处 `KNOWN_FEATURES` 漂移。Phase 4A 为 `WriteBatch` 增加受控的内部特性位更新方法；该方法只允许启用当前二进制已知位，并把候选特性位纳入现有 WAL `CommitState`。

状态规则：

| 特性位 | catalog_root | 解释 |
|---|---:|---|
| 未设置 | 0 | Phase 3 空逻辑数据库；允许读取为空目录或在可写事务中初始化 |
| 未设置 | 非 0 | 旧内部测试状态；Phase 4A 目录 API 不把它解释为 V1 目录 |
| 已设置 | 非 0 | 必须存在并完整验证的 V1 系统目录 |
| 已设置 | 0 | `Corruption`，不得静默重建 |

新数据库在首次需要可写目录时，以一个事务完成：

1. 确认特性位未设置且 `catalog_root == 0`。
2. 创建空目录 B+Tree。
3. 写入唯一 `CatalogMeta` 记录。
4. 登记目录树当前根并设置 `catalog_root`。
5. 启用 `CATALOG_V1`。
6. 通过现有 WAL 提交并发布完整状态。

只读打开未初始化数据库返回不可变的空目录视图，不分配页面、不创建 WAL 记录、不改变主文件。初始化中的任一错误或故障导致整个 `WriteBatch` 回滚；恢复后只能看到“未初始化”或“完整 V1 目录”之一。

## 7. 目录键格式

目录键采用字节序排序。记录族代码固定如下；未列出的代码均为保留值：

| 代码 | 记录族 | 键布局 |
|---:|---|---|
| `0x00` | Catalog meta | `00` |
| `0x10` | Table name | `10 + name_len:u16(be) + name:utf8` |
| `0x11` | Table definition | `11 + table_id:u64(be)` |
| `0x12` | Column name | `12 + table_id:u64(be) + name_len:u16(be) + name:utf8` |
| `0x13` | Column ordinal | `13 + table_id:u64(be) + ordinal:u16(be)` |
| `0x14` | Column definition | `14 + column_id:u64(be)` |

`0x20..=0x2f` 为 Phase 4C 索引记录保留，`0x30..=0x3f` 为约束记录保留。Phase 4A 不写入这些键，也不猜测其值格式。

名称长度使用大端 `u16`，但实际总键长仍受现有 1024 字节上限约束。ID 与序号使用大端是为了保持目录枚举顺序；目录值和行值中的数值继续遵守数据库固定小端格式。

映射值：

- Table name V1 载荷：精确 8 字节 `TableId:u64(le)`。
- Column name V1 载荷：精确 8 字节 `ColumnId:u64(le)`。
- Column ordinal V1 载荷：精确 8 字节 `ColumnId:u64(le)`。
- Definition 键值：对应版本化记录。

所有映射必须双向一致。打开目录时至少验证：每个名称/序号映射指向正确种类的定义、每个定义拥有唯一反向映射、表列序号连续、列的 `table_id` 与键前缀一致、名称在所属命名空间内唯一。

## 8. 目录值通用信封

所有目录值以 8 字节信封开始：

| 偏移 | 长度 | 字段 |
|---:|---:|---|
| 0 | 1 | record kind |
| 1 | 1 | record format version |
| 2 | 2 | reserved，必须为 0 |
| 4 | 4 | payload length，`u32(le)` |

记录总长度必须等于 `8 + payload_length`，不允许尾随字节。键族与 `record kind` 必须匹配。已知版本中的非法长度、保留位、枚举、UTF-8、ID 或交叉引用返回 `Corruption`；合法但高于当前实现的记录版本返回 `Unsupported`。

映射记录同样使用信封，避免裸 8 字节值被错误解释。记录种类代码与键族代码相同。

### 8.1 CatalogMeta V1

载荷：

| 字段 | 编码 |
|---|---|
| catalog format version | `u16(le)`，固定为 1 |
| reserved | `u16(le)`，固定为 0 |
| next object id | `u64(le)`，初始为 1，必须非零 |

目录中必须恰好存在一个 CatalogMeta。`next_object_id` 是下一次候选值；分配成功后以检查加法推进。

### 8.2 TableDef V1

载荷按顺序包含：

| 字段 | 编码与约束 |
|---|---|
| table id | `u64(le)`，非零且与键一致 |
| schema version | `u64(le)`，初始为 1 |
| root page id | `u64(le)`，非零 |
| root generation | `u64(le)`，非零 |
| next rowid | `u64(le)`，初始为 1 |
| column count | `u16(le)`，0..=1000 |
| name length | `u16(le)` |
| name | 指定长度的 UTF-8 字节，必须与名称映射一致 |

零列表在目录层允许，以便原子构建过程在提交前统一验证；已提交的 SQL 表是否允许零列由 Phase 4B/4C 决定。目录提交验证必须确认 `column_count` 与实际连续列记录数相同。

### 8.3 ColumnDef V1

载荷按顺序包含：

| 字段 | 编码与约束 |
|---|---|
| column id | `u64(le)`，非零且与键一致 |
| table id | `u64(le)`，必须引用现有 TableDef |
| ordinal | `u16(le)`，小于 1000 |
| column type | `u8`，固定枚举代码 |
| flags | `u8`，bit 0 表示 nullable，其余必须为 0 |
| name length | `u16(le)` |
| reserved | `u16(le)`，固定为 0 |
| name | 指定长度的 UTF-8 字节，必须与名称和序号映射一致 |

Timestamp 列类型区分无时区与固定偏移时区；解码时还必须验证具体 `Value::Timestamp` 是否与列类型的偏移存在性一致。

## 9. 对象 ID 与 ROWID 分配

### 9.1 对象 ID

`CatalogWriter::allocate_object_id` 读取批次可见的 CatalogMeta，返回当前 `next_object_id`，并在同一批次写回加一后的值。以下情况必须在写入任何目录记录前失败：

- 计数器为 0。
- 计数器已是 `u64::MAX`。
- 候选 ID 已存在于任一对象定义键。
- CatalogMeta 缺失、重复或格式错误。

创建表及其列时逐一分配 ID；只要最终批次未提交，所有分配均不可见，回滚后持久计数器保持原值。

### 9.2 隐藏 ROWID

每张表的 `next_rowid` 是下一次候选隐藏 ROWID：

- 从 1 开始，0 永远无效。
- 使用完整 `u64` 范围；达到 `u64::MAX` 时拒绝继续分配。
- 已提交 ROWID 在删除行后不复用。
- 回滚事务未发布的 ROWID 可以再次分配。
- ROWID 分配、TableDef 计数器推进和对应行插入必须位于同一 `WriteBatch`。

表 B+Tree 的物理行键是 `RowId` 的 8 字节大端编码。该编码使字节序扫描与数值 ROWID 顺序一致。Phase 4A 提供逐字节黄金测试，并拒绝非 8 字节键和零 ROWID。

## 10. 用户行磁盘格式

### 10.1 行头

V1 行头固定为 32 字节：

| 偏移 | 长度 | 字段 |
|---:|---:|---|
| 0 | 4 | magic，ASCII `OROW` |
| 4 | 2 | row format version，`u16(le)`，固定为 1 |
| 6 | 2 | flags/reserved，必须为 0 |
| 8 | 8 | schema version，`u64(le)`，非零 |
| 16 | 2 | column count，`u16(le)`，0..=1000 |
| 18 | 2 | null bitmap length，`u16(le)` |
| 20 | 4 | directory length，`u32(le)` |
| 24 | 4 | payload length，`u32(le)` |
| 28 | 4 | reserved，必须为 0 |

紧随行头的是 NULL 位图、列目录和载荷。要求：

- `null_bitmap_length == ceil(column_count / 8)`。
- 位图中超出列数的高位必须为 0。
- `directory_length == column_count * 8`。
- 总长度必须精确等于 `32 + bitmap + directory + payload`。
- 编码后整行长度不得超过调用方传入的 `max_row_bytes`，也不得超过 `u32::MAX`。

### 10.2 列目录

每列目录项固定 8 字节：

| 字段 | 编码 |
|---|---|
| payload offset | `u32(le)`，相对于载荷起点 |
| value length | `u32(le)` |

NULL 列的位图值为 1，目录项必须全 0。非 NULL 列的范围必须位于载荷内，按列序单调、连续、无重叠、无空洞。空 Text 或空 Blob 是非 NULL 值，长度可以为 0，其 offset 必须等于当时的连续载荷末尾。

严格连续的规范编码能够检测被篡改的重叠、乱序和隐藏字节。解码不得返回指向输入缓冲区之外的切片。

### 10.3 值载荷

值类型由 `ColumnDef` 决定，行内不保存类型标签：

| ColumnType | V1 载荷 |
|---|---|
| Int64 | 8 字节二进制补码 `i64(le)` |
| Decimal | flags `u8` + scale `i8` + reserved `u16(0)` + magnitude `u128(le)`，共 20 字节 |
| Float64 | `f64::to_bits()` 的 8 字节 `u64(le)` |
| Text | 严格 UTF-8 字节 |
| Blob | 原始字节 |
| Date | year `u16(le)` + month/day/hour/minute/second 各 `u8` + reserved `u8(0)`，共 8 字节 |
| Timestamp | Date 8 字节 + micros `u32(le)`，共 12 字节，必须无偏移 |
| TimestampWithTimeZone | Date 8 字节 + micros `u32(le)` + offset minutes `i16(le)` + reserved `u16(0)`，共 16 字节 |
| Bool | 单字节 0 或 1 |

Decimal flags 只允许 bit 0 表示负数；负零由现有 `Decimal` 构造器规范化。日期、微秒和时区偏移必须通过现有值构造器重新验证，不能仅按字节复制。Float64 保留精确位模式，包括 NaN 载荷；SQL 层如何比较非有限值不属于本阶段。

`OracleDate`、`OracleTimestamp` 和 `Decimal` 若缺少存储层所需字段访问器，只增加只读访问器，不公开可绕过构造校验的字段或反序列化后门。

RowCodec 要求调用方提供的模式版本、列数和列顺序与行头完全一致。当前阶段不存在合法的历史模式，因此任何已知 V1 行与当前 TableDef 模式版本不一致都返回 `Corruption`；未来实现 `ALTER TABLE` 时，只有被明确支持的旧模式版本才能进入升级路径。

## 11. 目录操作与事务不变量

Phase 4A 提供以下内部能力：

- `CatalogReader::open_empty_or_v1`
- `CatalogWriter::bootstrap`
- `find_table_by_name` / `find_table_by_id`
- `columns_for_table` / `find_column_by_name`
- `allocate_object_id`
- `allocate_rowid`
- `insert_table_definition`
- `insert_column_definition`
- `finish`

方法名在实现计划中可按 Rust 习惯微调，但行为边界不得改变。

创建一组表/列元数据时必须按以下逻辑执行：

1. 在批次视图中验证目录完整。
2. 验证名称和列序，不修改状态。
3. 创建或接收已经分配的用户表 B+Tree 根。
4. 分配所有对象 ID。
5. 写入 definition、name 和 ordinal 记录。
6. 更新 CatalogMeta。
7. 重新读取并验证批次候选目录。
8. 登记目录当前根与用户树根。
9. 提交现有 WAL 批次。

提交前验证的不变量包括：

- 目录中只有一个 CatalogMeta。
- 所有对象 ID 非零且种类唯一。
- 名称映射、定义和从属映射一一对应。
- 每张表的列序从 0 连续到 `column_count - 1`。
- 表根是合法、已登记且与目录/空闲页集合不重叠的 Table B+Tree 根。
- 目录根是合法、已登记且可由候选状态完整遍历的 B+Tree 根。
- `CATALOG_V1`、目录根和目录元记录状态一致。

Phase 4A 不对外提供单独提交目录对象的快捷方法。调用方必须持有 `WriteBatch`，因此未来 DDL 可以把目录变更和物理树变更放入同一 WAL 提交。

## 12. 读取、快照与恢复

`CatalogReader` 完全依赖传入的 `ReadSnapshot`。一个旧快照继续看到其 LSN 对应的目录根、TableDef、列列表和 `next_rowid`，即使新事务已经发布新的目录版本。不得从 Pager 的“最新状态”旁路读取目录元数据。

恢复沿用 Phase 3 规则：WAL 中只有拥有完整提交记录并成功同步的候选目录状态可发布。恢复发布前必须完整验证候选目录 B+Tree、记录信封和目录交叉引用，并浅验证每个用户表根的页号、代次和 Table B+Tree 根类型。验证失败返回 `Corruption`，不得截断到一个看似可用的更旧目录提交，除非 Phase 3 已经证明该后缀是未完成事务尾部。

普通打开和恢复不得遍历每张用户表的全部页面；否则打开成本会随 10 GB 数据规模线性增长。Phase 4A 另外提供专用深验证入口，在测试、故障证明和未来完整性检查中遍历目录引用的全部用户树，并验证所有用户页、目录页和空闲页集合互不重叠。正常写事务继续依赖 Phase 3 的受控页面分配/释放、代次校验和提交前 tracked-tree 验证；任何被错误复用的旧 PageRef 必须因代次不匹配返回 `Corruption`，不得静默读取新页面。

检查点从对应 `CommitState` 写回目录和用户页；不能用最新目录状态替换被旧快照需要的状态。

## 13. 错误分类

- `Type`：调用者构造的名称、列数、列类型、行值或资源上限无效；值与已绑定列类型不匹配。
- `Constraint`：同一命名空间重复表/列名称、重复列序或重复候选对象身份。
- `Transaction`：未初始化目录被只读写入、目录 writer 未正确完成、批次已结束或非法事务状态。
- `Unsupported`：磁盘记录或行使用合法但高于当前实现的格式版本。
- `Corruption`：已知版本长度、保留位、UTF-8、枚举、ID、映射、偏移、模式版本、根、代次或交叉引用无效。
- `Busy`、`Io`、`Internal`：继续保留 Phase 3 的锁、系统调用和内部不变量语义。

任何磁盘字节都不能导致 panic、越界、未检查整数运算或部分行返回。错误必须保留目录对象、表 ID、列序或行上下文，但不得把磁盘任意字节直接当作可信错误文本。

## 14. 测试设计

### 14.1 强类型和黄金编码

- 每个 ID 拒绝 0、保持显式种类转换。
- 每个目录键使用手工推导的精确字节断言，并证明 ID/序号扫描顺序。
- CatalogMeta、TableDef、ColumnDef 和映射信封逐字节黄金测试。
- ROWID 大端键覆盖 1、边界值和排序。
- V1 行头、NULL 位图、每种值载荷及混合行逐字节黄金测试。

### 14.2 行编解码

- 全部 `Value` 类型往返相等。
- NULL、空 Text、空 Blob 彼此不混淆。
- 0、1、8、9、999、1000 列边界。
- 类型不匹配、非空列 NULL、模式版本不匹配、资源上限和长度溢出。
- 截断每个固定字段和每个载荷边界。
- 非零保留位、未知类型/版本、非法 UTF-8、非法日期/小数/布尔值。
- 位图高位、目录越界、重叠、乱序、空洞、尾随字节和超平台长度。

### 14.3 目录模型

- 未初始化只读数据库返回空目录且文件字节不变。
- 可写 bootstrap 只创建一个目录元记录；重复 bootstrap 为幂等打开或明确状态错误，不重复分配根。
- 表/列创建、名称和 ID 查找、按序枚举、计数器推进。
- 重复名称、重复序号、悬空映射、缺失反向映射和非连续列序。
- 目录树根分裂后提交并重新打开仍能找到全部对象。
- 回滚后对象 ID、ROWID、目录根和记录均不可见。
- 旧快照在新目录提交后仍读取旧版本。
- 普通打开只浅验证用户表根，不随用户数据页总量线性扫描。
- 专用深验证遍历所有目录引用树，并拒绝用户页、目录页和空闲页集合重叠。
- 固定种子随机目录/行操作与 `BTreeMap` 参考模型一致。

### 14.4 WAL、故障与损坏

- 初始化、目录根分裂、TableDef/ColumnDef 批量写入和 ROWID+行写入的每个 WAL 故障边界。
- 子进程中止后只允许观察提交前或提交后的完整状态。
- 主文件/WAL 恢复后重复打开结果一致。
- 已设置特性位但根或 CatalogMeta 缺失必须失败，不得重建。
- 目录引用用户根与空闲页重叠、页代次错误、目录记录指向错误树类型必须失败。

### 14.5 持续集成

- Windows stable。
- Linux stable：格式、Clippy `-D warnings`、完整常规测试。
- Linux Rust 1.85 MSRV。
- Phase 4A 专用目录/行损坏和故障矩阵；若超过常规 CI 时间预算，使用与 Phase 3 相同的专用优化测试 profile，同时保留调试断言和溢出检查。
- 不提交 `Cargo.lock`、临时数据库、WAL、测试进程或生成目录。

## 15. 验收标准

进入 Phase 4B 前必须全部满足：

- 新建或 Phase 3 空数据库能够原子初始化 V1 目录。
- 只读打开未初始化空库不产生任何文件变化。
- 表/列定义、对象 ID、ROWID 和目录根在关闭重开及崩溃恢复后保持一致。
- 旧快照不会看到未来目录或未来 ROWID 计数器。
- 行格式覆盖全部现有内部值类型，NULL/空值语义无混淆。
- 单表 1000 列和调用方行大小上限得到受控处理。
- 所有已知损坏均返回受控错误；没有 panic、越界或静默错误数据。
- 故障注入证明目录、物理树和特性位不会部分发布。
- Windows、Linux、Rust 1.85、格式和 Clippy 门禁通过。
- 源码中没有 SQL 解析器、公共 CRUD、DDL/DML 执行、约束执行或 Phase 5 行为。

## 16. Phase 4B/4C 交接

Phase 4B 可以依赖以下稳定内部语义：

- 名称规范化完成后可按精确字节绑定表和列。
- 表、列及未来索引/约束使用稳定逻辑 ID。
- 目录读取服从事务级快照。
- 表模式版本和有序列定义足以驱动行编解码。
- SQL 参数和值在进入 RowCodec 前必须已经完成类型绑定和 Oracle 兼容转换。

Phase 4C 可以依赖以下稳定事务语义：

- 目录变更、B+Tree 根和用户行可处于同一 `WriteBatch`。
- 对象 ID 与 ROWID 计数器随提交原子发布，回滚不泄漏状态。
- 目录键空间为索引和约束记录族保留，但其实际格式只在行为与测试同时实现时确定。

Phase 4A 不提前冻结公共 SQL API，也不把未来表达式或约束表示塞入未被行为验证的磁盘记录。
