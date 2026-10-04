# ORA-SQLLite Phase 5 Oracle 类型设计

日期：2026-10-04

状态：已完成对话设计，等待书面规格复核

基础版本：`27ab42ec70f5a607ccb97d05604dc163d0e3ca87`

## 1. 目标

Phase 5 的第一阶段只加入 Oracle 风格的列类型声明和可执行类型契约，不加入查询函数、序列、`DUAL`、`ROWNUM` 或分页语法。

本阶段支持：

- `NUMBER(p[,s])`；
- `VARCHAR2(n)`；
- `CHAR(n)`；
- `RAW(n)`；
- `DATE`；
- `TIMESTAMP[(p)]`；
- `TIMESTAMP[(p)] WITH TIME ZONE`。

成功标准是这些声明不仅能被解析，还能被持久化并统一约束 INSERT、UPDATE、绑定参数、比较、约束和索引。旧 format 2 数据库无需启动迁移或全库重写即可继续使用。

## 2. 已批准的设计决定

1. Oracle 声明参数必须持久化并执行，不能只映射为 `Decimal`、`Text` 或 `Blob` 别名。
2. 所有目标列写入和目标列查询值都通过同一个 `normalize_for_column()` 边界。
3. 空 Text 值按 Oracle 语义转为 NULL；只包含空格的值不是空值。
4. 字符长度按 Unicode 标量值数量计算，不按 UTF-8 字节数或用户可见字形簇计算。
5. `CHAR(n)` 在写入时右补 ASCII 空格到 n 个字符。
6. `NUMBER(p,s)` 先按 scale 四舍五入，再检查 precision。
7. 时区类型只接受固定数字偏移，保存原始偏移。
8. 保持数据库 format 2，新增类型特性位和 `ColumnDef V2`；V1 与 V2 元数据可以共存。
9. 保留现有稳定 `ErrorCode`，常见兼容错误附加 ORA 编号。
10. 常规 CI 保持轻量；规模验收上限为 10 万行。

## 3. 非目标

以下能力不进入本阶段：

- Oracle 查询函数、转换函数或日期算术；
- 序列、触发器、PL/SQL、包或存储过程；
- `DUAL`、`ROWNUM`、Oracle 分页或层次查询；
- `NLS_DATE_FORMAT`、`NLS_NUMERIC_CHARACTERS` 或其他会话 NLS 状态；
- 地区时区名称、时区文件或夏令时规则；
- `TIMESTAMP WITH LOCAL TIME ZONE`；
- `NCHAR`、`NVARCHAR2`、LOB、INTERVAL、对象类型或集合类型；
- `VARCHAR2(n BYTE)`/`CHAR(n BYTE)` 以及可切换的 BYTE/CHAR 长度语义；
- 完整 Oracle 隐式转换矩阵；
- `ALTER TABLE` 或旧表在线类型迁移；
- 逐字复制 Oracle 错误消息；
- 100 万行、1000 万行或 10 GB 阻塞性规模门槛。

现有 `INT64`、`DECIMAL`、`FLOAT64`、`TEXT`、`BLOB` 和 `BOOL` 等项目原生声明继续使用 V1 宽松类型合同。

## 4. 架构与组件边界

```text
SQL parser / public parameters
              |
              v
OracleTypeDescriptor + binder target contract
              |
              v
normalize_for_column(column, value)
              |
              +--> normalized row value
              +--> constraint input
              +--> index search / index key input
              |
              v
Relation + IndexStorage + Catalog
              |
              v
WriteBatch / WAL / checkpoint / recovery
```

新增不可变的 `OracleTypeDescriptor`，表达已验证的声明：

```text
Number { precision, scale }
Varchar2 { length_chars }
Char { length_chars }
Raw { length_bytes }
Date
Timestamp { fractional_precision }
TimestampWithTimeZone { fractional_precision }
```

运行时 `ColumnDef` 同时提供现有物理 `ColumnType` 和可选 Oracle 描述符。V1 列没有描述符，行为保持不变；V2 列必须有与物理类型一致的描述符。

`normalize_for_column()` 是唯一的 Oracle 类型执行入口。SQL 执行器、关系层、约束和索引不得分别实现长度、舍入、补齐或空字符串规则。关系/行编码仍执行最终形状检查，但不能成为另一套类型语义。

列相关比较通过 `compare_for_contract()` 执行。现有 `sql_total_cmp()` 的 V1 行为保持不变，尤其不能把旧 TimestampWithTimeZone 从既有字段元组比较静默改成 UTC 比较。绑定器为 V2 谓词携带比较合同：NUMBER 按数值比较；VARCHAR2 按现有 Text 比较；两个 CHAR 按较长声明长度补齐后比较；CHAR 与 VARCHAR2 使用非补齐 Text 比较；无时区 Timestamp 按日历字段和纳秒比较；带时区 Timestamp 按 UTC 瞬间比较。不同 Oracle 类型族以及 V1/V2 列之间的列对列比较在 MVP 返回 Type；列与常量/参数则按目标 V2 列规范化。

查询计划中凡是把常量或参数与一列比较的可搜索条件，都保留目标列合同。执行时只规范化一次，同一个结果同时用于 IndexScan 范围和完整 Filter 重检，避免索引路径与 TableScan 产生不同答案。

## 5. 声明语法与边界

| 声明 | 参数规则 | 物理类型 |
|---|---|---|
| `NUMBER(p[,s])` | p 为 1..=38；s 默认为 0，范围 -38..=38 | Decimal |
| `VARCHAR2(n)` | n 为 1..=32767 | Text |
| `CHAR(n)` | n 为 1..=2000 | Text |
| `RAW(n)` | n 为 1..=32767 | Blob |
| `DATE` | 不接受参数或修饰符 | Date |
| `TIMESTAMP[(p)]` | p 默认为 6，范围 0..=9 | Timestamp |
| `TIMESTAMP[(p)] WITH TIME ZONE` | p 默认为 6，范围 0..=9 | TimestampWithTimeZone |

`NUMBER` 不带 precision 的无限制形式不进入 MVP，因为它需要不同于固定 precision/scale 的持久合同。`NUMBER(p,s)` 允许 s 大于 p，也允许负 s。

类型名和关键字继续大小写不敏感。缺失参数、额外参数、非整数参数、越界参数、未知修饰符和 `WITH LOCAL TIME ZONE` 在 CREATE TABLE 修改任何目录状态前失败。未知 Oracle 类型不得静默降级为 TEXT 或 BLOB。

一张新表可以同时包含 V1 原生声明和 V2 Oracle 声明。当前项目不支持 ALTER TABLE，因此本阶段没有把既有 V1 列改写为 V2 的执行入口；未来若加入该能力，必须以同一 WriteBatch 原子升级表元数据并保留旧列的 Legacy 合同。

## 6. 规范化语义

规范化顺序固定为：

1. 将 `Value::Text("")` 转成 `Value::Null`；
2. 若结果为 NULL，执行列可空性检查；
3. 执行目标导向且明确列出的转换；
4. 执行长度、precision、scale 和时区合同；
5. 产生唯一的规范值；
6. 使用该值执行 NOT NULL、CHECK、UNIQUE、PRIMARY KEY、FOREIGN KEY、行编码和索引编码。

空 Blob 不自动变成 NULL。空字符串规则只针对 Text 值；`" "` 和其他仅含空白的 Text 保留原值。

### 6.1 NUMBER

允许输入：

- `Value::Int64`；
- `Value::Decimal`；
- 使用 ASCII 数字、可选符号和一个小数点的 Text。

不接受 Float64 到 NUMBER 的隐式转换，避免二进制浮点输入造成不确定舍入。Text 解析不接受区域化分隔符、货币符号或指数记法。

目标 scale 为 s 时，使用十进制 half-away-from-zero 规则量化到 s。s 为负数时向十位、百位等舍入。量化可能进位；完成后规范 magnitude 的十进制有效数字数不得超过 p，零始终满足 precision。

示例：

- `NUMBER(5,2)` 的 `12.345` 规范为 `12.35`；
- `NUMBER(2,-1)` 的 `149` 规范为 `150`，内部可表示为 magnitude 15、scale -1；
- `NUMBER(2,1)` 的 `9.99` 先变成 `10.0`，随后因 precision 溢出失败；
- `NUMBER(4,5)` 的 `0.01234` 合法，而 `0.12345` precision 溢出。

规范后的 Decimal 继续使用现有数值比较和去尾零的 IndexKey 数值编码。因此数值相同而输入 scale 不同的值使用同一索引逻辑键。

### 6.2 VARCHAR2 与 CHAR

允许输入 Text。长度使用 Rust Unicode scalar count，也就是 `chars().count()`；UTF-8 多字节字符计为一个字符，组合字符按各自标量计数。

`VARCHAR2(n)` 保留原始非空 Text，不截断、不补齐。超过 n 个字符失败。

`CHAR(n)` 超长失败，较短时在右侧补 U+0020 到恰好 n 个字符。存储值、读取值、约束输入、比较参数和索引键都使用补齐后的 Text，因此 TableScan 和 IndexScan 一致。已有尾部空格不删除。

### 6.3 RAW

允许输入 Blob 参数或现有 `X'00AF'` 形式的十六进制字面量；解析器在进入规范化前已把该字面量转换为 Blob。十六进制大小写不敏感；奇数位或非十六进制字符在解析阶段失败。普通 Text 不隐式转换为 RAW。长度按解码后的字节数计算，超过 n 字节失败，不截断。

本阶段不增加 `HEXTORAW`、`UTL_RAW` 或字符集转换函数。应用程序绑定 RAW 时应优先使用 Blob 参数。

### 6.4 DATE

DATE 保存年、月、日、时、分、秒，不保存小数秒和时区。

允许输入 `Value::Date`，以及严格的 `YYYY-MM-DD` 或 `YYYY-MM-DD HH:MM:SS` Text；仅日期形式的时间为 `00:00:00`。不读取 NLS 状态，不接受模糊的本地化日期文本。

### 6.5 TIMESTAMP

无时区 TIMESTAMP 接受无偏移 `Value::Timestamp`，以及 `YYYY-MM-DD HH:MM:SS[.fraction]` Text。带时区列接受带固定偏移的 `Value::Timestamp`，以及 `YYYY-MM-DD HH:MM:SS[.fraction] +/-HH:MM` Text。

偏移范围为 `-14:00..=+14:00`，小时为 14 时分钟必须为 00。地区名称和夏令时折叠不支持。

小数秒按目标 p 使用 half-away-from-zero 舍入。进位必须正确跨越秒、分钟、日期和年份；超出支持日期范围时失败。p=0 不保存小数秒。

`TIMESTAMP WITH TIME ZONE` 的行值保留用户原始数字偏移。比较、唯一性和范围索引按换算后的 UTC 瞬间执行，因此表示同一瞬间的 `08:00 +08:00` 与 `00:00 +00:00` 相等；读取仍返回各自行中保存的原偏移。

## 7. 纳秒值与持久编码

现有 `OracleTimestamp` 只保存微秒，不能无损表达已批准的 p=7..9。本阶段把公共值域扩展为 0..999,999,999 的纳秒字段，并保留现有微秒 API：`OracleTimestamp::new(..., micros, ...)` 继续接受微秒并乘以 1000；新增纳秒构造器和只读 accessor。既有 `micros()` 返回完整微秒部分，也就是 `nanoseconds / 1000`，不会被持久编码调用来还原纳秒。

ColumnDef 版本决定 4 字节小数秒字段的单位：

- V1 Timestamp 行仍把该字段解释为 microseconds，并在读入值域时乘以 1000；
- V2 Timestamp 行把相同宽度字段解释为 nanoseconds；
- V1 写入继续要求纳秒能被 1000 整除并编码为 microseconds；
- V2 写入编码 nanoseconds。

因此无时区 Timestamp 行仍为 12 字节，带时区 Timestamp 行仍为 16 字节，不需要重写旧行，也不会发生 V1/V2 单位混淆。

索引逻辑结构仍为“列分量 + ROWID”，但 Timestamp 分量由列元数据选择编码：

- V1 Timestamp 使用既有 microseconds 编码，字节保持不变；
- V2 无时区 Timestamp 使用本地日历字段和 nanoseconds；
- V2 带时区 Timestamp 使用 UTC 日序与日内纳秒，使相同瞬间获得相同逻辑前缀。日序以 `0001-01-01 = 0`，使用翻转符号位的 i32 大端编码；日内纳秒使用 u64 大端编码；
- 偏移换算可能令边界日期得到 -1 或末日之后一天的 UTC 日序，这两类内部键合法；V2 时区索引解码器验证日序和日内纳秒后使用内部比较标量，不强行构造超出公共日期范围的 `OracleTimestamp`；
- 索引键不保存原偏移，SELECT 始终从表行返回原值。

上述扩展由新特性位保护。旧程序在读取目录、行或索引前已经拒绝数据库，因此不能把纳秒误解释为微秒。

## 8. ColumnDef V2 与特性位

新增：

```rust
const ORACLE_TYPES_V1: u64 = 1 << 2;
```

`ORACLE_TYPES_V1` 依赖 `CATALOG_V1`；它不要求数据库一定含有索引，但存在索引时继续遵守 `INDEX_CONSTRAINT_V1` 的既有规则。

ColumnDefinition 继续使用目录键族 `0x14`。记录信封 version 1 是既有 ColumnDef V1；version 2 是 ColumnDef V2。V2 payload 的所有多字节整数沿用目录 little-endian 编码，布局固定为：

| 偏移 | 宽度 | 字段 |
|---|---:|---|
| 0 | 8 | ColumnId u64 |
| 8 | 8 | TableId u64 |
| 16 | 2 | ordinal u16 |
| 18 | 1 | physical ColumnType u8 |
| 19 | 1 | nullable u8，仅 0/1 |
| 20 | 1 | Oracle kind u8 |
| 21 | 1 | precision u8 |
| 22 | 1 | scale i8 的二进制补码 |
| 23 | 1 | flags u8，必须为 0 |
| 24 | 4 | length u32 |
| 28 | 2 | name_len u16 |
| 30 | 2 | reserved u16，必须为 0 |
| 32 | name_len | UTF-8 列名 |

Oracle kind 编号固定为 NUMBER=1、VARCHAR2=2、CHAR=3、RAW=4、DATE=5、TIMESTAMP=6、TIMESTAMP WITH TIME ZONE=7。NUMBER 使用 precision/scale 且 length 必须为 0；三个长度类型只使用 length；两个 Timestamp 只使用 precision；DATE 的三个参数字段全为 0。其余未使用字段必须为 0。

解码器验证物理类型与描述符一致，例如 NUMBER 只能配 Decimal、RAW 只能配 Blob、带时区 Timestamp 必须配 TimestampWithTimeZone。未知 kind、未知标志、非法参数、尾随字节或不一致组合属于 Corruption。

兼容规则：

- 没有 `ORACLE_TYPES_V1` 的旧 format 2 数据库按原逻辑打开，不迁移、不重写；
- 首次成功创建至少一个 V2 列时，在同一 WriteBatch 写入 V2 目录记录并启用特性位；
- V1 和 V2 ColumnDefinition 可以位于同一数据库，乃至同一新表；
- V2 记录存在而特性位缺失属于 Corruption；
- 特性位存在但没有 V2 记录是合法状态，例如最后一个 V2 表已被删除；
- 特性位一旦提交不清除；
- 不认识该位的旧引擎必须在恢复、截断或目录读取前返回 Unsupported；
- 只读打开不能启用该位或补写记录。

## 9. SQL、参数、约束与索引数据流

### 9.1 INSERT

1. 求值 SQL 值或读取参数；
2. 按每个目标 ColumnDef 规范化；
3. 构造完整规范行；
4. 执行 NOT NULL、CHECK、PRIMARY KEY、UNIQUE 和 FOREIGN KEY；
5. 从同一规范行生成所有索引键；
6. 写表、索引和目录根；
7. 通过既有语句检查点和 WAL 原子提交。

### 9.2 UPDATE

每行先完成表达式求值，再按目标列规范化完整新行。全部规范化、约束和索引键预检通过后，才能删除旧索引项和替换表行。后续行失败时，既有外层语句检查点恢复本语句此前的全部修改。

### 9.3 查询参数与谓词

绑定器保留列目标合同，并允许本规格列出的源值形状延迟到执行期转换；不能由现有宽类型参数检查提前拒绝合法的 Text-to-NUMBER、Text-to-Date 或 Text-to-Timestamp 输入。执行参数在 TableScan 或 IndexScan 启动前规范化一次：

- 空 Text 参数因此可以成为 NULL；
- NUMBER 参数使用目标 precision/scale；
- CHAR 参数使用相同补齐规则；
- Timestamp 参数使用相同 p 和 UTC 索引语义。

规范化结果既用于构建索引范围，也用于完整 WHERE Filter。普通 NULL 比较仍遵守三值逻辑；本阶段不改变 `NULL = NULL` 为 UNKNOWN 的规则。

CHECK 中直接与 V2 列比较的持久常量在 CREATE TABLE 编译时按该列合同规范化。V2 列对列 CHECK 使用同一 `compare_for_contract()` 规则；这需要给持久 CheckProgram 增加受 `ORACLE_TYPES_V1` 保护的比较合同操作码或等价版本化负载，不能改变既有 V1 操作码的含义。

### 9.4 外键

为了避免父子列使用不同舍入、补齐或 UTC 规则，MVP 中 V2 外键的父子列必须有完全相同的 OracleTypeDescriptor。V1 外键维持现有兼容规则；V1 与 V2 列之间不建立外键。所有拒绝发生在 CREATE TABLE 写目录之前。

## 10. 错误模型

现有稳定 `ErrorCode` 保持不变。`DbError.oracle_code()` 作为可选兼容元数据，不参与事务致命性判定。

| 情况 | ErrorCode | ORA 编号 |
|---|---|---|
| NUMBER 文本无效 | Type | 01722 |
| NUMBER 舍入后 precision 溢出 | Type | 01438 |
| VARCHAR2、CHAR 或 RAW 超长 | Type | 12899 |
| 日期/时间文本与严格格式不匹配 | Type | 01861 |
| NOT NULL 失败，包括空字符串变 NULL | Constraint | 01400 |
| PRIMARY KEY/UNIQUE 冲突 | Constraint | 00001 |
| 已识别类型的参数或组合非法 | Type | 00902 |
| 已明确排除的类型或修饰符 | Unsupported | 00902 |
| 未知特性位 | Unsupported | 无 |
| V2 目录、行或索引损坏 | Corruption | 无 |

消息不逐字复制 Oracle，但必须说明列名、目标声明和失败原因。ORA 编号用无前导零的数值保存，展示层可以格式化为五位。

普通 Type/Constraint 错误恢复当前语句，显式事务可按既有规则继续；Corruption、Io 和 Internal 沿用现有致命事务规则。任何转换失败都不得留下部分表、索引、目录或特性位修改。

## 11. 快照、恢复与旧库行为

- `ORACLE_TYPES_V1` 与 V2 ColumnDef 在同一 CommitState 发布；崩溃后只能看到完整旧状态或完整新状态。
- WAL CommitState、Checkpoint 和恢复继续携带完整单调 feature bits，不新增日志文件或旁路元数据。
- 旧读快照继续使用其 V1/V2 ColumnDef 和旧表/索引根；不能观察未来类型变更。
- 正常打开只验证目录结构、描述符和可达性，不扫描全表重新规范化数据。
- V1 Timestamp 行和索引必须用黄金文件证明字节完全不变。
- V2 解码遇到未知版本、保留位、非法参数、非法 nanoseconds 或非法偏移时安全失败，不能猜测或降级。
- 备份与恢复仍以数据库文件和 WAL 为单位，不产生必须同步复制的外部 schema 文件。

## 12. 测试设计

### 12.1 单元测试

- 每种声明的合法、默认、下界、上界和越界解析；
- ColumnDef V2 黄金字节、每字节截断、未知版本、保留位和严格 EOF；
- NUMBER 正负值、half 边界、负 scale、进位和 precision 溢出；
- Unicode 标量长度、CHAR 补齐、空字符串和仅空格字符串；
- RAW 十六进制、奇数位、非法字符和字节上限；
- DATE/TIMESTAMP 严格文本、p=0..9、跨秒/日/年进位和偏移边界；
- V1 microseconds 与 V2 nanoseconds 行/索引黄金字节；
- 同一 UTC 瞬间不同偏移的比较和索引前缀；
- 每项 ErrorCode 与 ORA 编号映射。

### 12.2 CRUD 与路径一致性

- 每种新类型覆盖 INSERT、SELECT、UPDATE、DELETE 和重开；
- SQL 字面量与绑定参数产生相同规范值和错误；
- TableScan 与 IndexScan 返回相同结果；
- NOT NULL、CHECK、PRIMARY KEY、UNIQUE 和 FOREIGN KEY 只观察规范值；
- 同一值的更新不产生伪唯一冲突；
- 失败语句的表行、全部索引项和影响行数恢复到语句前状态。

### 12.3 格式、故障与恢复

- 无新特性位的真实旧 format 2 文件只读/读写；
- V1/V2 表和列共存；
- 首次启用特性位的目录页、数据页、索引页、Commit、WAL sync 和目录 sync 故障矩阵；
- 模拟旧引擎对未知位的明确拒绝；
- 特性位存在但 V2 记录为空的合法数据库；
- V2 记录缺位、参数损坏、单位越界及特性位缺失的受控失败；
- Checkpoint 和重新打开后 precision、scale、CHAR 补齐、nanoseconds 与原始时区偏移不变。

### 12.4 CI 与规模

常规 CI 只运行：

- 快速单元测试；
- Oracle 类型 CRUD/索引/约束集成测试；
- Linux stable 基础构建与测试；
- Rust 1.85 MSRV 构建；
- Windows 基础构建与测试。

10 万行作为手工或发布前规模验收，不进入每次 CI。规模用例覆盖批量写入、索引创建或维护、目标查询、更新/删除和关闭后重开。验收只要求正确完成且无断言、panic、损坏或数据不一致，不把共享机器吞吐时间设为阻塞阈值。

不再运行或要求 100 万、1000 万行及 10 GB 测试。既有大型工具可以保留为非阻塞诊断资产。

## 13. 建议的交付边界

后续实现计划应拆成可独立验证的顺序任务，而不是一个大改动：

1. OracleTypeDescriptor、声明绑定、ColumnDef V2 和特性位；
2. NUMBER、字符和 RAW 规范化；
3. 纳秒 Timestamp 值域、V1/V2 行编码和日期文本；
4. INSERT/UPDATE、参数、约束与错误元数据接入统一规范化；
5. V2 索引键、IndexScan 参数规范化和 UTC 时区语义；
6. 旧库、故障恢复、Windows/Linux/MSRV 和 10 万行验收。

每项都必须先写失败测试，再做最小实现；计划阶段可以进一步细分提交，但不得改变本规格的磁盘合同或类型语义。

## 14. 退出条件

Phase 5 Oracle 类型阶段完成必须满足：

- 七类 Oracle 声明均能解析、持久化、重开并执行参数合同；
- 空字符串、Unicode 字符长度、CHAR 补齐、RAW 长度和 NUMBER 舍入只有一个实现来源；
- TIMESTAMP p=0..9 无损，V1 microseconds 数据无回归；
- 带时区值保存原偏移，同时比较和索引遵守 UTC 瞬间语义；
- INSERT、UPDATE、参数、约束、TableScan 和 IndexScan 使用同一规范值；
- 稳定 ErrorCode 未改变，规定场景返回对应 ORA 元数据；
- 旧 format 2 数据库无需迁移，新类型特性位和 V2 记录原子发布；
- 未知版本、未知位和损坏参数被控制地拒绝；
- 常规 Linux、Rust 1.85 MSRV 和 Windows 验证通过；
- 10 万行手工规模验收通过；
- 源码中不存在本规格非目标中的函数、序列、NLS、地区时区或 ALTER TABLE 行为。
