# ADR 003：页存储用页镜像 WAL，先提交后写数据文件

状态：**Proposed**（checkpoint 扩展见 ADR 004；本文保留页存储的基础提交/恢复决策）

日期：2026-11-20

## 背景

`tritdb-txn` 已经实现 key/value 事务（现在是单 WAL 写者 + 并发逻辑事务），但它的事务状态是内存 `BTreeMap`，WAL 记录是逻辑操作（`Put`/`Delete`），没有页文件、页缓存和页级 redo。要做 B+ 树、表堆或列存块，必须有下一条垂直切片：8 KiB 页文件、单写页事务、崩溃后只重放已提交页。

选择点：

1. 页存储复用 `tritdb-txn` 的 WAL 和事务对象，还是独立 WAL 消费者？
2. WAL 记录逻辑操作还是页镜像？
3. 提交先写数据文件还是先 fsync WAL？
4. 崩溃留下的半页尾是报错、忽略，还是截断后重放？

## 决策

新增 `tritdb-store`，作为**独立的页级 WAL 消费者**。

- 数据文件：第 `N` 页偏移 `N * 8192`，没有 superblock；文件长度正常必须是 8192 的倍数。
- WAL：复用 TRWL v0 记录。`Put` payload 为 `page_id u64 LE` + 8192 字节页，共 8200 字节；`Begin`/`Commit`/`Abort` payload 为空；`Delete` 在页存储 WAL 中不支持。
- 提交顺序：追加 `Commit` → fsync WAL → 写数据文件 → fsync data → 更新缓存 → 推进 `page_count` → 释放写者锁。
- 恢复：先截断数据文件非 8192 整数倍的尾部，再扫描 WAL；只重放带 `Commit` 的事务；未提交 `Begin/Put` 丢弃；恢复后对 data 再 fsync。
- 缓存：`RwLock<HashMap<PageId, Page>>`；命中直接返回；未命中释放 cache 读锁后读盘，再填 cache；M0 无淘汰、无上限。
- 锁顺序：writer `WAL -> data -> cache`；reader 命中只拿 cache 读锁；miss 先放 cache 再拿 data，最后拿 cache 写锁填充。
- 单写者：`PageTxn` 从 `begin` 到 `commit`/`abort` 持有 WAL `MutexGuard`；`PageStore` 是 `Send + Sync`，`PageTxn` 不是 `Send`。

## 理由

- **页镜像是 redo 的最小闭环**。逻辑 `Put` 需要页内布局、空闲空间管理、索引维护才能重放；页镜像只需要“把这张 8 KiB 页写回去”，适合 M0 先验证崩溃一致性。
- **先 WAL 后数据**让“已提交”成为可恢复的承诺。若先写数据再 fsync WAL，崩溃可能留下数据页但 WAL 没有提交记录，恢复会丢掉已返回成功的事务。
- **截断半页尾**比报错更符合崩溃语义。半页不可能是唯一副本：提交前页镜像已经 fsync 到 WAL；截断后重放已提交页即可修复。
- **独立消费者**避免把 key/value 事务和页事务硬塞进一个 `TxnStore`。两者恢复单位不同：前者按逻辑 key 聚合，后者按页镜像重放；M0 先保持边界清晰。
- **缓存无淘汰**是刻意简化。M0 先验证正确性；buffer pool、pin/unpin、脏页回写和淘汰策略留给后续 ADR。

## 后果

正面：

- 页存储有真实的 8 KiB 页文件、页缓存、单写页事务和硬杀恢复测试。
- 崩溃后页 id 连续、payload 对齐、已提交页不丢；半页尾可自动修复。
- 提交路径的瓶颈被量出来：一次提交两次 fsync，1 页 median 5.040 ms，8 页 5.327 ms，fsync 主导。

负面：

- WAL 体积约为数据页大小；每次提交写 8200 字节/页，WAL 增长快。
- 没有 checkpoint 时每次打开都全量重放已提交页；checkpoint 由 ADR 004 补上后，只重放 checkpoint 之后的记录。
- 缓存无上限，长跑会吃内存；没有脏页回写，所有页写都在提交路径。
- 没有跨进程文件锁；两个进程同时打开会互相破坏。
- 没有页分配位图；CLI 只能追加新页或更新已有页，不能安全地稀疏写。
- 没有快照隔离；跨页一致视图不在 M0 承诺内。

## 备选方案

1. **复用 `TxnStore` 的 key/value WAL**：把页编码成 key/value。问题是页镜像 8 KiB 会放大 key/value 抽象，恢复要按 key 重组页，且 `TxnStore` 的状态是内存 map，不适合大页文件。放弃。
2. **逻辑操作 WAL**：记录“在页 P 偏移 O 写 N 字节”。WAL 更小，但需要页内布局和幂等重放；M0 没有页内格式。推迟。
3. **先写数据文件再 fsync WAL**：崩溃会留下未提交数据页，恢复无法判断哪些页该保留；违反“提交才可见”。放弃。
4. **半页尾直接报错**：硬杀测试会在数据写入中间失败，恢复不可用；且半页本来就可由 WAL 修复。放弃。
5. **引入 checkpoint 立即做**：当时需要页分配位图、WAL 截断、恢复起点和并发控制，超出第一条切片；后来由 ADR 004 用 sidecar + 单段 reset 实现，仍不做段轮转。

## 后续

- checkpoint + WAL 段轮转：checkpoint 已由 ADR 004 实现（单段 reset）；段轮转仍待做。
- 组提交：把多个事务的 `Commit` 合并为一次 fsync，目标是把 3 ms 级提交降到亚毫秒。
- 页分配位图 + 空闲页回收。
- 跨进程文件锁，或明确单进程约束。
- 页级 MVCC/快照：至少提供“一个读事务看到一致的页前缀”。
- B+ 树页内布局：在页镜像之上加索引页分裂/合并。
