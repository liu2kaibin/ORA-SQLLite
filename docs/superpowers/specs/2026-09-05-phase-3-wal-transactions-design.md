# ORA-SQLLite Phase 3 WAL、事务与恢复设计

**日期：** 2026-09-05  
**状态：** 已完成交互式设计，等待书面规格确认  
**前置基线：** Phase 2 存储引擎，提交 `f0511af`  
**目标 MSRV：** Rust 1.85

## 1. 目标

Phase 3 为现有 Pager、完整页镜像和 B+Tree 增加可靠的预写日志（WAL）、崩溃恢复、事务级一致性读快照、进程内多读单写、Checkpoint 和跨进程独占文件锁。

完成后必须满足：

- `commit()` 只有在完整 WAL 事务耐久同步后才返回成功。
- 所有已报告成功的提交在进程或机器重启后仍然存在。
- 未提交事务和 WAL 尾部半写事务在恢复后不可见。
- 读事务固定于创建时的提交 LSN，不观察未来提交。
- 同一进程允许多个读事务与一个写事务并存。
- 第二个进程不能同时打开同一数据库，必须返回 `Busy`。
- Checkpoint 可以安全重试，且任何中断点都不丢失已提交事务。

Phase 3 仍然保持 crate 内部接口，不发布 SQL 或公共数据库 CRUD API。

## 2. 已确认的产品边界

### 2.1 包含

- 完整页镜像 WAL。
- 固定、版本化、带校验的 WAL 和双 Superblock 格式。
- 单写者与事务级读快照。
- WAL 优先的 Pager 读取路径。
- 崩溃恢复、尾部处理、Checkpoint 和安全 WAL 重置。
- Windows 与 Linux 跨进程独占文件锁。
- WAL、缓存和活跃快照的资源上限。
- 故障注入、子进程强制终止、并发压力和恢复模型测试。

### 2.2 不包含

- SQL、解析器、规划器、执行器或系统目录模式。
- 公共 `Database`、`Connection`、`Statement` 或 `Transaction` API。
- 跨进程并发读写。
- 多写者、嵌套事务或 `SAVEPOINT`。
- 逻辑日志、undo 日志或 ARIES 恢复。
- 异步 I/O、`mmap`、后台 Checkpoint 线程或网络服务。
- Phase 2 试验性数据库文件迁移器。
- `NORMAL`、`OFF` 等弱持久化模式。

### 2.3 持久化约定

首版只支持全持久化：页面帧和 Commit Record 写入 WAL 后，必须完成 WAL 的 `sync_all`，事务才可以发布并向调用方报告成功。

`sync_all` 返回错误时，提交结果对调用方是失败且不得在当前进程发布。由于操作系统可能在报告 I/O 错误前写入部分或全部数据，关闭并重新打开时，恢复器仍以磁盘上的完整 Commit Record 和校验结果为唯一事实来源。调用方不得在收到 I/O 错误后假定该事务一定不存在。

## 3. 总体架构

```text
                     private mutable pages
WriteBatch --------------------------------------+
    | validate complete candidate state          |
    v                                            |
WAL page frames -> Commit Record -> sync WAL      |
                                      |           |
                                      v           |
                           publish LSN + WalIndex |
                                      |           |
ReadSnapshot(snapshot_lsn)             |           |
    |                                 |           |
    +-> WalIndex newest <= snapshot ---+           |
    +-> main file at checkpoint_lsn if no WAL page|
                                                   |
Checkpoint (serialized with writer) <--------------+
    -> write main pages -> sync main
    -> publish alternate Superblock -> sync main
    -> when no readers remain, safely reset WAL
```

现有 B+Tree、slotted page、overflow 和 freelist 算法继续通过 `PageSource` 与 `WriteBatch` 工作，不感知 WAL 文件布局。

主要内部对象：

- `DatabaseStorage`：持有进程级文件锁、主文件、WAL、共享提交状态和缓存。
- `Wal`：追加记录、同步、定位读取、截断未提交尾部和安全重置。
- `WalIndex`：按 `PageId` 保存已提交页面版本的 LSN 与文件偏移。
- `SnapshotRegistry`：登记活跃读快照及引用数，计算最老活跃 LSN。
- `ReadSnapshot`：持有固定 LSN，并在释放时注销。
- `WriteBatch`：持有唯一写锁、私有脏页、候选 Superblock 状态和基础 LSN。
- `Checkpoint`：在写锁下选择安全 LSN，将对应状态写回主文件。

## 4. 主数据库文件格式 2.0

Phase 3 将内部磁盘格式主版本从 1 升至 2。格式 1 是未公开的 Phase 2 试验格式；打开流程先识别文件偏移 0 的旧 magic，并明确返回 `Unsupported`，不把它误报为双 Superblock 损坏。测试和开发数据库必须重新创建。

### 4.1 固定 Superblock 区

主文件开头保留固定 8192 字节，不属于普通数据页：

- Slot A：文件偏移 `0..4096`
- Slot B：文件偏移 `4096..8192`

两个 Slot 分处独立的 4 KiB 区域。普通页面偏移为：

```text
8192 + (page_id - 1) * page_size
```

`PageId(0)` 继续表示“无页面”并禁止普通页 I/O。第一个普通页面为 `PageId(1)`。逻辑 `page_count` 保持 Phase 2 语义：仅有 Superblock、尚无普通页时为 1；合法普通页面必须满足 `1 <= page_id < page_count`。

物理文件长度必须等于：

```text
8192 + (page_count - 1) * page_size
```

### 4.2 Superblock Slot 编码

每个 Slot 恰好 4096 字节，全部整数使用小端序：

| 偏移 | 长度 | 字段 | 约束 |
|---:|---:|---|---|
| 0 | 16 | magic | `ORA-SQLLITE-SB` 加两个 NUL 字节 |
| 16 | 2 | major | `2` |
| 18 | 2 | minor | `0` |
| 20 | 4 | page_size | 4096、8192 或 16384 |
| 24 | 16 | database_uuid | 创建时生成后不变 |
| 40 | 8 | publish_sequence | 从 1 开始严格递增 |
| 48 | 8 | checkpoint_lsn | 主文件完整包含到此 LSN |
| 56 | 8 | page_count | 逻辑页数，最小为 1 |
| 64 | 8 | catalog_root | 0 或合法普通页面 |
| 72 | 8 | freelist_root | 0 或合法普通页面 |
| 80 | 8 | wal_generation | 从 1 开始，WAL 重置后递增 |
| 88 | 8 | feature_bits | 未知位必须拒绝 |
| 96 | 3996 | reserved | 必须为零 |
| 4092 | 4 | crc32c | 对 `0..4092` 计算 |

`reserved` 实际覆盖 `96..4092`。解码器必须要求完整 4096 字节、固定 magic、已知版本、合法字段、全零保留区和正确 CRC32C。

打开数据库时独立解码两个 Slot：

1. 两个均无效：返回 `Corruption`。
2. 仅一个有效：使用该副本。
3. 两个有效且 UUID 或页大小不同：返回 `Corruption`。
4. 两个有效且状态一致：选择 `publish_sequence` 较高者。
5. `publish_sequence` 相同但状态不同：返回 `Corruption`。

数据库创建必须先写 Slot A 并同步，再写 Slot B 并再次同步。两个初始 Slot 状态相同，发布序号分别为 1 和 2。

## 5. WAL 文件格式 1.0

WAL 路径为主数据库路径追加 `-wal`。例如 `app.db` 对应 `app.db-wal`。

### 5.1 双 WAL Header Slot

WAL 同样保留固定 8192 字节 Header 区，记录从偏移 8192 开始：

- Header Slot A：文件偏移 `0..4096`
- Header Slot B：文件偏移 `4096..8192`

每个 Header Slot 恰好 4096 字节：

| 偏移 | 长度 | 字段 | 约束 |
|---:|---:|---|---|
| 0 | 16 | magic | `ORA-SQLLITE-WAL` 加一个 NUL 字节 |
| 16 | 2 | major | `1` |
| 18 | 2 | minor | `0` |
| 20 | 4 | page_size | 必须等于 Superblock |
| 24 | 16 | database_uuid | 必须等于 Superblock |
| 40 | 8 | wal_generation | 与已发布 Superblock 配对 |
| 48 | 8 | base_checkpoint_lsn | WAL 创建或重置时的 checkpoint |
| 56 | 16 | salt | 每次重置使用新的非零随机值 |
| 72 | 8 | header_sequence | 从 1 开始严格递增 |
| 80 | 4012 | reserved | 必须为零 |
| 4092 | 4 | crc32c | 对 `0..4092` 计算 |

两个 Slot 独立校验。正常打开必须选择 `wal_generation` 与已发布 Superblock 相同的有效 Slot；同 generation 有两个有效 Slot 时选择 `header_sequence` 较高者。相同 sequence 但内容不同是 `Corruption`。

数据库创建必须在返回成功前创建 WAL，并依次同步两个与当前 Superblock 匹配的空 Header Slot；初始 Slot 的 sequence 分别为 1 和 2。之后 WAL 即使为空也必须保留。打开已有数据库时 WAL 缺失返回 `Io`，不能假定主文件已经包含最新提交。唯一修复例外是数据库仍处于可证明的初始空状态：`checkpoint_lsn=0`、`page_count=1` 且两个根均为 0；读写打开可以补建空 WAL，只读打开仍不修改文件。

有效 WAL Header 必须满足 `base_checkpoint_lsn <= Superblock.checkpoint_lsn`。小于当前 checkpoint 表示 WAL 仍保留已经写回主文件的旧提交；恢复器继续验证这些记录，但不把 `lsn <= checkpoint_lsn` 的页面加入活动索引。

### 5.2 Record Header

双 WAL Header 区之后是连续记录，不做预分配。每条记录由 64 字节 Record Header 和 `payload_len` 字节 Payload 组成；记录总长度补零到 8 字节边界，padding 必须为零。

| 偏移 | 长度 | 字段 | 约束 |
|---:|---:|---|---|
| 0 | 8 | magic | `ORAWREC` 加一个 NUL 字节 |
| 8 | 1 | record_type | 1=PageFrame，2=Commit |
| 9 | 1 | flags | 必须为 0 |
| 10 | 2 | header_len | 必须为 64 |
| 12 | 4 | payload_len | 由记录类型限定 |
| 16 | 8 | transaction_lsn | 大于 base checkpoint |
| 24 | 4 | ordinal | PageFrame 从 0 连续递增；Commit 等于帧数 |
| 28 | 4 | reserved_1 | 必须为 0 |
| 32 | 8 | page_id | PageFrame 非零；Commit 为 0 |
| 40 | 8 | generation | PageFrame 非零；Commit 为 0 |
| 48 | 4 | payload_crc32c | 对 Payload 计算 |
| 52 | 4 | header_crc32c | 此字段置零后对 64 字节 Header 计算 |
| 56 | 8 | wal_generation | 必须等于当前 WAL Header |

任意未知记录类型、非零保留字段、非法长度、算术溢出、越界、错误 padding 或 CRC 错误均为 `Corruption`，不能 panic 或分配不受控内存。

### 5.3 PageFrame 记录

- `payload_len` 必须恰好等于数据库页大小。
- Payload 是已经通过 `PageFrame::prepare_for_write` 和完整解码复验的页面镜像。
- `page_id` 和 `generation` 必须与 Payload 页头一致。
- 一个事务内 PageFrame 按 `PageId` 严格升序写入，不得重复。
- 事务不能记录 `PageId(0)`。

### 5.4 Commit 记录

Commit Payload 固定为 64 字节：

| 偏移 | 长度 | 字段 | 约束 |
|---:|---:|---|---|
| 0 | 8 | previous_lsn | 等于前一已提交 LSN |
| 8 | 8 | final_page_count | 候选逻辑页数 |
| 16 | 8 | catalog_root | 候选目录根 |
| 24 | 8 | freelist_root | 候选空闲链根 |
| 32 | 4 | frame_count | 等于 Commit ordinal |
| 36 | 4 | transaction_crc32c | 按写入顺序覆盖全部 PageFrame Header 与 Payload |
| 40 | 8 | feature_bits | 必须仅包含已知位 |
| 48 | 16 | reserved | 必须为零 |

Commit Record 自身的 Header CRC 和 Payload CRC 之外，再用 `transaction_crc32c` 检测缺帧、重复、乱序和事务范围拼接。

空写事务不写 WAL、不分配 LSN，并直接成功结束。

### 5.5 LSN 规则

- 初始 `checkpoint_lsn` 为 0，第一个提交 LSN 为 1。
- LSN 在一个数据库内严格递增且不得重复。
- 每个事务的 LSN 必须等于 `previous_lsn + 1`。
- `u64::MAX` 后禁止新提交并返回 `Transaction`，不回绕。
- WAL 重置不重置 LSN，只更新 `wal_generation` 和 `base_checkpoint_lsn`。

## 6. 提交协议

写事务开始时获取唯一写锁，并捕获当前 `latest_lsn`。所有 B+Tree 修改、分配、释放和候选根更新继续存在 `WriteBatch` 私有状态中。

提交步骤不可重排：

1. 完成 freelist 准备和现有全树、页面、溢出链及空闲链校验。
2. 计算候选逻辑页数和候选根状态，并执行资源上限检查。
3. 为事务分配 `latest_lsn + 1`，但此时不发布。
4. 将脏页按 `PageId` 升序编码为 PageFrame 记录。
5. 追加 Commit Record。
6. 对 WAL 调用 `sync_all`。
7. 在一次短临界区内把全部帧加入 `WalIndex`，更新最新状态和 `latest_lsn`。
8. 用不可变页面替换相应最新版本缓存项。
9. 释放写锁并返回成功。

在第 6 步成功前，读事务不能看到新事务。第 6 步完成后到第 7 步之间如果进程终止，重启恢复必须发布该事务；调用方没有收到成功返回，但数据库状态仍以耐久 Commit Record 为准。

WAL 追加或同步失败时：

- 不修改已发布 LSN、索引或缓存。
- 标记写侧为 `NeedsRecovery`，拒绝后续写事务。
- 已存在和新建的读快照仍可读取最后已发布状态，因为未提交尾部不进入索引。
- 关闭并重新打开时扫描并处理尾部，恢复到 `Ready`。

## 7. 恢复协议

打开顺序：

1. 获取主文件跨进程独占锁。
2. 解码并选择有效 Superblock。
3. 打开 WAL；验证 UUID、页大小、版本、generation 和 base checkpoint。
4. 从偏移 8192 顺序、有限地扫描记录。
5. 以事务为单位暂存 PageFrame 元数据，不在看到 Commit 前发布。
6. 验证连续 LSN、连续 ordinal、页面升序、全部 CRC、事务摘要和候选状态。
7. Commit 完整有效后，原子加入恢复中的 `WalIndex` 并更新最新状态。
8. 扫描结束后校验恢复出的目录树和 freelist 可达性。

尾部处理规则：

- EOF 位于 Record Header、Payload 或 padding 中间：从该事务第一条记录处截断。
- EOF 位于若干完整 PageFrame 之后但没有 Commit：截断整个未提交事务。
- 完整 Header/Payload 的 CRC 错误、非法字段、错误顺序或 Commit 摘要不匹配：返回 `Corruption`，不截断掩盖错误。
- 在一个无效记录之后发现额外完整字节：返回 `Corruption`。
- 只允许截断最后一个未提交事务，已发布事务范围永不自动丢弃。
- WAL 重置中断后，当前 Superblock 已发布新 generation、但记录区仍以旧 generation 开始时，整个记录区是可证明的旧代数据；读写打开将其截断到 8192 字节并同步，只读打开忽略它。旧代记录之后若出现当前代记录则返回 `Corruption`。

读写打开在截断尾部后必须同步 WAL。只读打开不得修改尾部；它忽略可证明只是 EOF 截断的最后未提交事务，但对其他损坏仍返回 `Corruption`。

重复打开和恢复必须产生相同 `latest_lsn`、页面索引、候选根和查询结果。

## 8. 读快照与页面缓存

### 8.1 线程安全页面句柄

当前 `Rc<RefCell<PageFrame>>` 替换为 `Arc<PageFrame>`。已发布页面不可变；写事务需要修改时复制为自己拥有的 `PageFrame`。这同时满足：

- 页面可在线程间安全共享。
- 老读快照持有的页面不会被新提交原地修改。
- 不再存在 `RefCell` 动态借用 panic 路径。

### 8.2 快照登记

`SnapshotRegistry` 保存 `BTreeMap<Lsn, usize>` 引用数：

- 开始读事务时，在提交状态读锁内捕获 `latest_lsn` 并增加引用数。
- `ReadSnapshot` 释放时减少引用数，归零后移除 LSN。
- 最老活跃 LSN 是映射的第一个键。
- 引用计数溢出返回 `Internal`，不得回绕。

### 8.3 版本选择

`WalIndex` 为每个 PageId 保存按 LSN 递增的 `WalVersion { lsn, offset, generation }`，并按 LSN 保存 `CommitState { page_count, catalog_root, freelist_root, feature_bits }`。Checkpoint 必须从 CommitState 取得目标 LSN 的完整数据库状态，不能用最新状态代替旧快照状态。

读取 `PageId` 时：

1. 查找该页面 `lsn <= snapshot_lsn` 的最新 WAL 版本。
2. 命中则按固定偏移读取、复验 Record 和 PageFrame，再返回缓存的 `Arc<PageFrame>`。
3. 未命中则从主文件读取当前 Checkpoint 页面。

缓存键必须包含 `(PageId, source_lsn)`；主文件版本使用当前 `checkpoint_lsn`。不同快照需要的页面版本可同时留在有界缓存中。缓存满且所有候选项均被外部句柄固定时返回 `Busy`。

## 9. 并发和锁顺序

`DatabaseStorage` 可以由 `Arc` 在线程间共享。逻辑并发保证是多个读事务可以与一个写事务同时存在；首版不承诺无锁缓存或并行磁盘读取。

固定锁顺序：

1. 进程级主文件锁（对象整个生命周期持有）。
2. writer/checkpoint 互斥锁。
3. 提交状态读写锁。
4. 快照登记锁。
5. 页面缓存锁。

任何代码不得逆序获取锁。WAL 或主文件 `sync_all` 期间可以持有 writer 锁，但不能持有提交状态、快照或缓存锁。

写事务在创建时取得 writer 锁并持有到提交、回滚或 Drop。Drop 未完成的写事务只清理内存私有状态，不写 WAL。

跨进程锁使用 `fs4 = 1.1.0` 的同步 `FileExt` 独占锁：

- 对读写和只读数据库打开都申请主文件独占锁。
- 锁冲突映射为 `ErrorCode::Busy`。
- 锁随 `DatabaseStorage` 关闭和文件句柄释放而释放。
- 项目自身继续 `#![forbid(unsafe_code)]`。
- `fs4 1.1.0` 的 MSRV 为 Rust 1.75，低于项目 Rust 1.85 下限。

## 10. Checkpoint 协议

Checkpoint 与写事务共用同一 writer 锁，因此不会与提交并行；读事务不被停止。

### 10.1 目标 LSN

- 没有活跃读快照：目标为 `latest_lsn`。
- 存在活跃读快照：目标为最老活跃快照 LSN。
- 目标不大于当前 `checkpoint_lsn` 时无需写主文件。

对每个在 `(checkpoint_lsn, target_lsn]` 内出现的 PageId，写回不高于目标 LSN 的最新版本。然后同步主文件数据页。

### 10.2 Superblock 发布

数据页同步成功后：

1. 构造 `checkpoint_lsn = target_lsn` 的候选 Superblock。
2. `publish_sequence` 加 1；溢出时返回 `Corruption` 并停止写入。
3. 将候选状态写入当前较旧或无效的 Slot。
4. 同步主文件。
5. 内存中发布新的主文件基线。

任一步失败时，另一个旧 Slot 仍然定义安全 Checkpoint；WAL 保留该点后的全部已提交页面，因此重新打开可以恢复。

### 10.3 WAL 回收

首版不复制或移动仍被快照需要的 WAL 后缀：

- 只要存在活跃读快照，Checkpoint 可以前进，但 WAL 文件不物理缩短。
- 当没有活跃读快照且 Checkpoint 已达到 `latest_lsn`，可以完整重置 WAL。

完整重置顺序：

1. 已完成主文件页面同步和 `checkpoint_lsn = latest_lsn` 的 Superblock 发布。
2. 在较旧的 WAL Header Slot 写入 `wal_generation + 1`、`base_checkpoint_lsn = latest_lsn`、递增 sequence 和新 salt，然后同步 WAL；此时旧记录区保持不动。
3. 发布一个仅更新 `wal_generation`、发布序号加 1 的 Superblock，并同步主文件。
4. 截断 WAL 到 8192 字节并同步，保留双 Header Slot。
5. 清除不再需要的 WalIndex 版本、CommitState 和旧版本缓存。

如果在步骤 2 后中断，Superblock 仍选择旧 generation 和旧 WAL Header，旧记录保持可恢复。如果在步骤 3 后、步骤 4 前中断，Superblock 选择新 generation，而记录区中的旧 generation 使整段数据可证明为待丢弃的旧代记录。任何阶段都不需要根据无效或缺失 WAL 猜测是否存在较新提交。

### 10.4 自动触发和上限

- WAL 文件达到 64 MiB 后，在下一次写事务开始前尝试 Checkpoint。
- WAL 硬上限为 1 GiB。
- 长读事务阻止完整重置且下一事务会超过硬上限时，新写事务返回 `Busy`，错误上下文指出最老快照 LSN。
- 单个事务自身超过 1 GiB 时返回 `Transaction`，且在任何 WAL I/O 前失败。
- 所有长度、偏移、帧数和 LSN 运算使用检查算术。

## 11. 文件与模块边界

文件布局固定为：

- `src/storage/wal/mod.rs`：WAL 生命周期和对 Pager 的窄接口。
- `src/storage/wal/format.rs`：WAL Header、Record Header、PageFrame 和 Commit 编解码。
- `src/storage/wal/index.rs`：已提交页面版本索引和安全裁剪。
- `src/storage/wal/recovery.rs`：顺序扫描、事务聚合、尾部分类和恢复结果。
- `src/storage/checkpoint.rs`：目标选择、页面写回、双 Superblock 发布和 WAL 重置。
- `src/storage/snapshot.rs`：快照登记与 RAII 句柄。
- `src/storage/header.rs`：格式 2 双 Superblock Slot 编解码与选择。
- `src/storage/file.rs`：8192 字节前缀、普通页偏移、共享读取、锁和同步边界。
- `src/storage/cache.rs`：`Arc<PageFrame>` 和带版本缓存键的有界 LRU。
- `src/storage/pager.rs`：读快照、写覆盖层、WAL 提交与 Checkpoint 集成。
- `src/fault.rs`：细分 WAL、主文件、Superblock、截断和同步故障点。
- `src/storage/test_support.rs`：子进程场景、模型状态和临时数据库/WAL 清理。

模块依赖方向固定为：格式和基础文件 I/O不依赖 Pager；WAL 索引和恢复依赖格式；Pager 依赖 WAL、快照和 Checkpoint；B+Tree 只依赖 `PageSource` 与写覆盖接口。

## 12. 故障状态和错误映射

| 情况 | 错误码 | 行为 |
|---|---|---|
| 第二进程持锁 | `Busy` | 不打开数据库 |
| 第二写者或 WAL 被长快照顶到上限 | `Busy` | 不产生 WAL I/O |
| 非法事务状态或单事务超限 | `Transaction` | 保留已发布状态 |
| WAL/Superblock 格式或校验损坏 | `Corruption` | 拒绝打开或提交 |
| 文件写入、同步、截断失败 | `Io` | 写侧进入 `NeedsRecovery` |
| 未知磁盘版本或格式 1 文件 | `Unsupported` | 不尝试迁移 |
| 锁中毒或不可能的内部状态 | `Internal` | 拒绝继续写入 |

所有错误必须携带操作名称、路径类别、LSN、PageId 或 WAL 偏移等可用上下文，但不得把正常锁竞争或未提交尾部伪装成损坏。

## 13. 测试策略

### 13.1 固定编码测试

- 两个 Superblock Slot 的固定偏移、小端字节、全零保留区和 CRC 黄金向量。
- WAL Header、两种 Record Header、Commit Payload 和 8 字节 padding 黄金向量。
- 每个长度、计数、偏移和 LSN 边界的溢出测试。
- 格式 1 文件明确返回 `Unsupported`。

### 13.2 恢复与损坏测试

- 在 Record Header、Payload、padding 和 Commit 的每个字节边界截断。
- 缺帧、重复 PageId、ordinal 间隙、LSN 间隙、错误 previous LSN、错误摘要。
- WAL 与数据库 UUID、页大小、generation 不匹配。
- 已提交帧和 Commit 的 Header/Payload CRC 损坏必须返回 `Corruption`。
- 未提交 EOF 尾部只在允许规则下被忽略或截断。
- 重复恢复、重复 Checkpoint 和恢复后再次提交结果一致。

### 13.3 子进程崩溃矩阵

独立子进程在以下边界强制终止，父进程重新打开验证：

- 每个 PageFrame 写入前后。
- Commit Record 写入前后。
- WAL 同步前后。
- 内存发布前后。
- 每个主文件页面写回前后。
- 主文件数据同步前后。
- 两个 Superblock Slot 写入和同步前后。
- WAL 截断、Header 重写和同步前后。

每个场景必须只得到旧的完整提交状态或新的完整提交状态，不能出现结构不一致、静默丢失已报告成功的提交或 panic。

### 13.4 快照与并发模型

- 老读快照在多个后续提交后仍读取原版本。
- 新读快照立即读取最新已发布版本。
- 写事务读取自己的私有修改，其他读事务不可见。
- 多个线程随机执行开始读、扫描、提交、回滚和 Checkpoint，与串行参考模型对比。
- 长读事务期间 Checkpoint 不回收所需版本。
- 所有读者释放后 WAL 可以完整重置。
- 第二进程持锁、释放和异常退出后的重新打开覆盖 Windows 与 Linux。

### 13.5 CI

快速门禁继续包含：

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-targets --all-features`
- Linux stable
- Linux Rust 1.85 MSRV
- Windows stable

子进程崩溃矩阵和较长并发模型放入独立 Linux CI Job，避免拖慢快速反馈；Windows 必须保留文件锁、重新打开、删除临时 WAL 和基本崩溃恢复覆盖。

## 14. Phase 3 验收门槛

进入 Phase 4 前必须全部满足：

- 所有报告成功的事务在正常关闭和强制终止后均可恢复。
- 所有未提交事务在恢复后不可见。
- 恢复与 Checkpoint 幂等。
- 已知损坏被检测并返回受控错误，无 panic、越界或静默错误数据。
- 事务级读快照不观察未来提交。
- 多读单写压力测试无死锁、脏读、提交丢失或版本回收错误。
- 长读事务存在时 WAL 不被错误重置；硬上限稳定阻止无限增长。
- 第二进程打开同一文件稳定返回 `Busy`。
- 缓存、WAL 索引和快照登记结构都有可证明的边界或生命周期回收。
- Linux stable、Rust 1.85、Windows stable 和独立崩溃恢复 CI 全部通过。
- `README.md` 明确更新为 Phase 3 能力和限制。
- crate 继续 `#![forbid(unsafe_code)]`。
- 没有 SQL、目录模式、公共 CRUD API 或 Phase 4 行为混入。

## 15. Phase 4 交接

Phase 4 可以在本层之上公开 `Database`、`Connection` 和事务 API，并把系统目录、DDL、基础 CRUD、约束与参数绑定放入同一 WAL 提交。Phase 4 只能依赖以下稳定内部语义：

- 一个写事务要么不发布，要么以单个 LSN 完整发布。
- 读快照在生命周期内保持事务级一致。
- 目录根、空闲链根和数据页面在同一 Commit Record 中原子更新。
- Checkpoint 和 WAL 重置不改变逻辑查询结果。
- 进程或机器终止后，打开数据库即可完成确定性恢复。

Phase 3 不提前设计 SQL 层接口；它只提供满足上述语义的内部存储事务边界。
