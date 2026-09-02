# ORA-SQLLite Phase 2 存储引擎设计

状态：已完成对话设计确认，等待书面规格审核
日期：2026-09-02

## 1. 目标

Phase 2 在 Phase 1 固定磁盘格式原语之上实现可持久化的内部存储引擎：

- 单数据库文件的创建、打开、定长页读写和同步。
- Pager、容量受限的页缓存、页面分配和回收。
- 槽式页面、变长单元和大值溢出页链。
- 通用字节键值 B+Tree 的查询、范围扫描、插入、分裂、删除、借位、合并和根变化。
- 正常提交并关闭后重新打开，数据保持一致。
- 所有损坏输入返回受控错误，不发生 panic、越界访问或静默返回错误数据。

初始容量目标保持为单库不超过 10 GiB、单表最终可扩展至 1000 万行。Phase 2 证明结构正确性和可扩展性，不承担最终规模验收。

## 2. 明确不包含的能力

Phase 2 不实现：

- WAL、崩溃原子提交、启动恢复和检查点。
- 多读单写并发、跨线程共享、文件锁和多进程同时打开。
- 系统目录、表定义、索引定义、ROWID 分配和约束。
- SQL、Oracle 语法、规划器、执行器和公开数据库 CRUD API。
- 加密、压缩、内存映射文件和异步 I/O。

这些边界意味着：一次提交在全部文件同步成功后可用于正常关闭再打开；进程在多页写入中途终止时，文件可能包含部分结果。Phase 3 必须通过 WAL 消除这一限制。在此之前，存储引擎保持 crate 内部可见，不能作为安全事务 API 对外发布。

## 3. 分层与模块

```text
B+Tree
  -> Slotted Page / Overflow Chain
  -> WriteBatch / Page Cache / Freelist
  -> Pager
  -> DatabaseFile
```

新增模块：

- `src/storage/file.rs`：创建、打开、精确页 I/O 和同步。
- `src/storage/pager.rs`：页面校验、读取、分配、回收和写批次。
- `src/storage/cache.rs`：容量受限的已验证页面缓存。
- `src/storage/slotted.rs`：槽目录和变长单元管理。
- `src/storage/freelist.rs`：SQLite 式 trunk/leaf 空闲页链。
- `src/storage/overflow.rs`：大值溢出链。
- `src/storage/btree/mod.rs`：公共内部 B+Tree 门面和节点路由。
- `src/storage/btree/search.rs`：点查和游标定位。
- `src/storage/btree/mutate.rs`：插入、删除和树再平衡。
- `src/storage/validate.rs`：页面、树和空闲空间的一致性检查。

Phase 1 的 `checksum.rs` 应拥有 CRC32C 实现，`storage/mod.rs` 只负责模块路由和共享新类型。所有新接口使用 `pub(crate)`、`pub(super)` 或私有可见性，不从 `lib.rs` 重新导出。

继续只使用 Rust 标准库，保持 Rust 1.85 MSRV 和 `#![forbid(unsafe_code)]`。

## 4. 文件打开与版本策略

### 4.1 创建

`DatabaseFile::create` 使用 `create_new`，不得覆盖现有文件。调用方提供：

- 路径。
- 页大小：4096、8192 或 16384。
- 16 字节数据库 UUID。
- 最大数据库字节数，默认 10 GiB。

第 0 页是完整页大小的数据库头页：0–127 字节使用 Phase 1 `DatabaseHeader`，剩余字节必须写零。创建完成前执行 `sync_all`。Phase 2 不负责生成加密安全 UUID。

### 4.2 打开

打开时先精确读取 128 字节数据库头，解码页大小，再验证：

- 文件至少包含一个完整页。
- 文件长度是页大小的整数倍。
- 页大小和数据库头 CRC32C 有效。
- 所有根页号小于当前页数或为 0。
- 已知主版本为 1，未知主版本拒绝。

新增 `OpenMode::{ReadOnly, ReadWrite}`。格式次版本大于当前支持版本时：

- `ReadOnly` 只有在所有特性位均已知时允许打开。
- `ReadWrite` 返回 `ErrorCode::Unsupported`。

`DatabaseHeader` 增加只读访问器和受限的根页更新方法；调用方不能任意修改版本、UUID或特性位。

### 4.3 Phase 2 根页持久化

Phase 2 尚无系统目录，因此不尝试持久化任意数量的命名 B+Tree。B+Tree 操作返回当前 `PageRef`，调用方负责保存根引用。

为了验证关闭后重新打开，Phase 2 的文件级集成测试允许把一个内部测试树的 PageId 写入数据库头 `catalog_root`：

- `WriteBatch::set_catalog_root(PageRef)` 先验证该引用是当前批次可达的有效树根，再只把 PageId 写入数据库头。
- 重新打开时通过 `catalog_root` 读取目标页，并从该页头恢复 generation，组成完整 PageRef。
- 当前根页在新数据库头同步前不得释放或复用。
- `catalog_root` 在 Phase 2 不代表已经实现系统目录；Phase 4 将用真正的目录树接管该字段并定义多树注册。

未写入 `catalog_root` 的 B+Tree 只在其 PageRef 由调用方持有期间可访问，不承诺跨重新打开自动发现。

## 5. 页地址与完整页校验

- PageId 0 永远表示数据库头页，普通页面从 1 开始。
- 文件偏移使用 `page_id * page_size` 的受检 `u64` 运算。
- 超过文件页数、最大数据库大小或发生整数溢出时返回受控错误。
- 每次读取普通页必须取得完整 `page_size` 字节；短读或截断视为 `Corruption`。
- 普通页 CRC32C 计算覆盖完整页面，但计算时将页头 36–39 字节的 checksum 字段视为零。
- 读取顺序：完整长度检查、整页 CRC32C、`PageHeader` 解码、请求 PageId 匹配、页面类型和代次约束。
- 写入顺序：验证内存页面结构、将 checksum 字段置零、计算 CRC32C、回填 checksum、再执行精确写入。

加载失败的页面不得进入缓存。

`DatabaseHeader` 和 `PageHeader` 为 Pager 增加字段访问器及保持不变量的受限更新方法；不得把字段改为 crate 公共可写。整页 checksum 的设置只由 Pager 执行。

## 6. 页面引用与代次

新增内部类型：

```rust
struct PageRef {
    page_id: PageId,
    generation: u64,
}
```

- 文件尾追加的新页从 generation 1 开始。
- 被回收页再次分配时 generation 加 1；溢出返回错误。
- B+Tree 子页、溢出页下一跳和空闲 trunk 下一跳都保存 `PageRef`。
- 读取引用目标时，目标页头 generation 必须匹配。
- Phase 1 页头只为右兄弟保存 PageId，因此叶子页面正文元数据另存右兄弟 generation。

这样可以检测已回收页被旧引用重新访问的情况。

## 7. 页缓存

`PageCache` 默认容量为 1024 页，允许在 `PagerConfig` 中修改。默认内存占用随页大小约为 4、8 或 16 MiB。

- 缓存键为 PageId，值为已通过校验的不可变 `PageFrame`。
- 缓存使用确定性的 LRU 顺序。
- `PageHandle` 使用 `Rc<RefCell<PageFrame>>` 管理 Phase 2 单线程固定页；缓存自身持有一个引用。
- 强引用数量大于 1 的页面视为已固定，不得淘汰。
- 只缓存干净页面；写批次的私有副本不计入可淘汰缓存。
- 容量已满时淘汰最久未使用且未固定的页面。
- 所有页面均被固定时返回 `ErrorCode::Busy`。

Phase 3 引入线程共享时可以在不改变 Pager/B+Tree 语义的前提下替换句柄内部实现。

## 8. WriteBatch 与正常提交

`Pager::begin_write(&mut self)` 返回独占的 `WriteBatch`。同一 Pager 同时只能存在一个批次。

写批次维护：

- 首次修改时复制的页面镜像。
- 新分配页面及其代次。
- 待释放页面。
- B+Tree 新根、空闲链根等数据库头更新。
- 确定性的脏页集合。

在任何文件 I/O 开始前，逻辑错误可通过 `rollback` 或丢弃批次完全撤销。`commit` 顺序为：

1. 验证全部修改页面以及受影响树结构。
2. 必要时安全扩展文件。
3. 为每个数据页计算整页校验和并按 PageId 顺序写入。
4. 执行 `sync_all`。
5. 最后写入更新后的数据库头页。
6. 再次执行 `sync_all`。
7. 将提交后的页面加入或替换缓存并结束批次。

文件写入或同步开始后发生错误，批次返回 `Io`，Pager 进入 `Poisoned` 状态；该实例不得继续读写，只能关闭并重新打开。Phase 2 不声称能够回滚已经写出的部分页面。

所有写入和同步调用前后使用 `Arc<dyn FaultInjector>` 检查 Phase 1 定义的故障点。使用 `write_all` 处理合法短写；故障测试验证写入和同步错误被传播且 Pager 被毒化。

## 9. SQLite 式空闲页链

数据库头的 `freelist_root` 指向首个 trunk 页面；0 表示没有空闲页。

Freelist trunk 页面使用 `PageType::Freelist`。页面正文固定前缀保存：

- 下一 trunk 的 PageRef，0/0 表示链尾。
- 当前 leaf PageId 数量。
- 保留零字节。
- 后续连续的 64 位空闲 PageId 数组。

规则：

- 释放普通页时，优先将 PageId 压入根 trunk。
- 根 trunk 已满时，被释放的页面自身初始化为新 trunk，并指向旧根。
- 分配时从根 trunk 弹出 leaf PageId。
- 根 trunk 不含 leaf 时，根 trunk 自身成为返回页，根指针前移。
- 同一批次释放的页面不得在该批次立即重新分配。
- PageId 0、当前 trunk、仍被树或溢出链引用的页面不得进入空闲链。
- 重新分配时清零完整页面并递增 generation。

该结构避免使用 B+Tree 管理 B+Tree 自己所需的页面，从而消除递归分配依赖。

## 10. 槽式页面

槽条目固定 4 字节：`offset: u16` 和 `len: u16`。槽目录从页面正文元数据之后向高地址增长，单元内容从页尾向低地址增长。

节点元数据：

- 叶子节点：页头后保存右兄弟 generation，首个槽从偏移 56 开始。
- 内部节点：页头后保存最左子页 PageRef，首个槽从偏移 64 开始。

`SlottedPage` 提供：

- 按槽读取单元。
- 在指定排序位置插入。
- 删除槽和单元。
- 页面内压缩，消除碎片。
- 计算连续和压缩后的可用空间。
- 重新编码时更新 `cell_count`、`slot_end` 和 `payload_start`。

所有 offset/len 加法使用受检运算。每个槽必须位于页面范围内，槽之间不得重叠，正文不得覆盖页头、节点元数据或槽目录。

## 11. B+Tree 单元格式

Phase 2 B+Tree 只处理按字节词典序排序的唯一物理键。

- 最大键长度为 1024 字节。
- 重复键插入返回 `ErrorCode::Constraint`。
- 后续普通索引通过“编码业务键 + ROWID”形成唯一物理键。

叶子单元保存：键长度、值存储类型、总值长度、键字节，以及内联值或首个溢出 PageRef。能放入当前页面的值内联保存；否则整个值写入溢出链，不实现内联前缀优化。

内部单元保存：分隔键长度、右子页 PageRef 和分隔键字节。页面元数据单独保存最左子页。内部分隔键始终等于右侧子树的最小键。

编码器必须拒绝：

- 超过 1024 字节的键。
- 无法容纳最小合法单元的页面。
- 长度不能安全转换为磁盘整数的值。

## 12. 溢出页链

`PageType::Overflow` 页面正文保存：下一页 PageRef、当前块长度、保留零字节和数据块。叶子单元保存总值长度与首个 PageRef。

读取溢出值时：

- 每跳验证 PageId、PageType、generation、整页 checksum 和块长度。
- 使用已访问 PageId 集合检测循环。
- 最大跳数不得超过文件普通页总数。
- 累计字节数不得超过叶子单元声明长度。
- 链结束时累计字节数必须精确等于声明长度。

删除或替换大值时，原链页面加入当前批次的待释放集合，并在后续批次中才允许复用。

## 13. B+Tree 行为

内部门面：

```rust
struct BTree {
    kind: TreeKind,
    root: PageRef,
}

enum TreeKind { Table, Index }
```

主要内部操作：

- `create(batch, kind)`
- `get(pager, key)`
- `insert(batch, key, value)`
- `delete(batch, key) -> Option<Vec<u8>>`
- `scan(pager, start, end) -> BTreeCursor`

规则：

- 点查在内部节点选择小于首个分隔键时走最左子页，否则选择最后一个小于等于目标键的分隔键右子页。
- 范围扫描定位首个叶子和槽，然后沿右兄弟链前进。
- 叶子分裂按编码后字节占用选择最平衡边界，右页首键插入父节点。
- 内部页分裂提升中间分隔键，并为左右页设置正确的最左子页。
- 非根节点有效载荷低于可用空间 25% 时触发再平衡。
- 优先从相邻同父节点借入一个或多个单元；供体借出后仍须满足 25% 下限。
- 无法借位且合并后可容纳时执行合并；单个合法大单元导致的低占用不视为错误。
- 根只有一个子节点时收缩为该子节点；空树保留一个空叶根。
- 根变化首先更新 `BTree.root` 并记录到 WriteBatch；只有调用方显式选择持久化到 `catalog_root` 时，数据库头才在数据页成功同步后更新。

`BTreeCursor` 持有对 Pager 的独占可变借用，逐页返回结果，避免把完整范围加载到内存。

## 14. 结构验证器

`validate_tree` 用于测试、调试和提交前受影响子树检查。它验证：

- 所有页面引用在文件范围内且 generation 匹配。
- 节点 PageType 与 TreeKind/层级一致。
- 每页键严格递增。
- 父分隔键等于右子树最小键。
- 所有叶子深度一致。
- 右兄弟链无循环、顺序正确且覆盖全部叶子。
- 每个可达树页只出现一次。
- 槽、单元和正文区域不重叠且在页内。
- 溢出链完整、无循环且长度准确。
- 空闲页链无循环、无重复，且与树页、溢出页不相交。

验证器发现任何不确定状态均返回 `Corruption`，不得尝试猜测修复。

## 15. 错误语义

- 文件权限、磁盘空间、操作系统读写和同步失败：`Io`。
- 截断文件、校验和错误、非法页面引用、代次不符、循环和结构破坏：`Corruption`。
- 未知次版本数据库的写打开：`Unsupported`。
- 页缓存全部被固定、暂时无法淘汰：`Busy`。
- 非法键长度和无法编码的单元：`Type`。
- 重复物理键：`Constraint`。

错误必须保留原始操作上下文，例如 PageId 和操作名称；Phase 2 不要求复制 Oracle 错误文本。

## 16. 测试策略

### 16.1 单元测试

- 文件创建、拒绝覆盖、页偏移受检计算和精确读写。
- 数据库头版本打开策略。
- 整页 checksum 计算和各种字节损坏。
- LRU 淘汰、固定页保护、全固定 Busy 和容量上限。
- 槽插入、删除、压缩、边界和重叠损坏。
- Freelist trunk 满/空转换、页面复用和 generation 递增。
- 溢出链的多页值、循环、截断、代次错误和替换释放。
- B+Tree 每种分裂、借位、合并、根增长和根收缩。

### 16.2 模型对比测试

使用标准库 `BTreeMap<Vec<u8>, Vec<u8>>` 作为参考模型和固定种子的简单伪随机发生器。在三种页大小下执行可复现的插入、查询、删除和范围扫描序列，每个操作后比较结果，并周期性运行 `validate_tree`。

### 16.3 持久化测试

- 提交后销毁 Pager，重新打开文件并比较全部键值。
- 多次打开/关闭不改变文件内容和树结构。
- 删除后页面被后续批次复用，文件不会因重复增删持续无效增长。

### 16.4 故障与损坏测试

- 在文件写入和同步故障点返回错误，确认错误传播和 Pager Poisoned 状态。
- 截断数据库、翻转页面字节、篡改页号/代次/槽/链指针，确认只返回受控错误。
- Phase 2 不将子进程强制终止后的文件状态作为通过条件；该门槛属于 Phase 3 WAL。

### 16.5 规模测试

- 常规 CI 使用中等、确定性数据集，控制总时间。
- 百万级小记录测试标记为手动或忽略测试，记录文件大小、树高、页复用和运行时间。
- 1000 万行和 10 GiB 正式验收保留到 Phase 6。

## 17. Phase 2 完成门槛

Phase 2 只有在以下条件全部满足后才能进入 WAL 设计：

- Linux stable、Rust 1.85 和 Windows stable CI 通过。
- 三种页大小的模型对比、持久化和损坏测试通过。
- B+Tree 覆盖查询、扫描、插入、删除、分裂、借位、合并和根变化。
- 整页 checksum、代次、溢出链和空闲链验证均已启用。
- 页缓存容量可证明受限，固定页不被淘汰。
- 正常提交和关闭后的重新打开结果一致。
- 公开文档明确说明没有 WAL 时不保证崩溃原子性。
- 没有 SQL、公开数据库 API、并发事务或 Phase 3 行为混入。

## 18. Phase 3 交接

Phase 3 WAL 将复用 `PageRef`、完整页镜像、WriteBatch 脏页集合和 FaultInjector。届时：

- WriteBatch 不再直接把数据页作为事务提交点，而是先生成 WAL 页帧。
- Pager 读取路径在主文件前查询 WAL 快照。
- Poisoned 状态由 WAL 恢复和提交边界替代。
- 单写者锁、读快照、检查点和恢复成为新的对外正确性门槛。

Phase 2 不为未来 WAL 预写空实现，但接口必须保持脏页集合和文件 I/O 边界清晰，使 Phase 3 能替换提交策略而不重写 B+Tree。
