# ADR 006：并发逻辑事务 + 提交组

状态：Proposed
日期：2026-11-20

## 背景

ADR 002 的 M0 事务层让 `TxnStore::begin` 持有 `Mutex<WalWriter>` 直到提交。这样只有一个活跃写事务，提交路径每次都要 `fsync`：Windows 上约 3.12 ms/次，1,024 个事务就是 3 秒级。M0 读数把 fsync 列为提交路径的绝对瓶颈。

组提交的前提不是“多个事务共享一个线程”，而是：

- 多个逻辑事务可以在 WAL 中交错 `Begin`/`Put`/`Delete`；
- 每个事务的 `Commit` 记录先进入队列，不立即 fsync；
- 一个 leader 对一批 `Commit` 做一次 `fsync`，成功后再按 WAL 顺序应用状态。

这仍然保持 **单 WAL 写者**：同一时刻只有一次 WAL append，文件只有一个写句柄。变化的是 WAL 锁的持有范围，不是写者数量。

## 决策

### 1. WAL 锁只包住单次 append

`TxnStore::begin`、`Txn::put`、`Txn::delete` 各自拿一次 `Mutex<WalWriter>`，追加记录后立即释放。`Txn` 不再持有 `MutexGuard`。多个逻辑事务可以交错追加记录。

代价是丢失了“事务独占写者”的简单性：两个事务可以写同一个 key，最终值由 WAL 顺序决定，最后提交者覆盖前者。M0 不做冲突检测。

### 2. Commit 进入队列，leader 批量 fsync

`Txn::commit`：

1. 持有 WAL 锁追加 `Commit`，但不 fsync；
2. 在仍持有 WAL 锁时把 `CommitWork { seq, commit_lsn, ops }` 放进 `CommitQueue`；
3. 释放 WAL 锁；
4. 若队列没有 leader，把自己选为 leader；否则等待 `synced_seq >= my_seq`。

leader：

1. 先让出调度器 `GROUP_COMMIT_YIELDS = 32` 次，给其他提交者入队的机会；
2. 拿 WAL 锁，再拿队列锁，取走当前全部 `CommitWork`；
3. 调一次 `WalWriter::sync()`；
4. 按 `commit_lsn` 升序，把每个事务的 ops 应用到 `RwLock<BTreeMap>`；
5. 设置 `synced_seq = max(seq)`，清空 leader，`notify_all`；
6. 释放队列锁和 WAL 锁。

**入队必须在持有 WAL 锁时完成**。否则存在窗口：Commit 记录已追加、leader 已 fsync 并取走队列，但该事务尚未入队；它会被误判为已同步，状态却没有应用。这个顺序是提交组正确性的关键不变量。

### 3. 等待与下一次 leader

被唤醒的提交者检查 `synced_seq >= my_seq`。如果自己的 seq 还没覆盖，且当前没有 leader，就自己成为下一任 leader。这样不需要单独的 writer 线程；等待者本身完成推进。

### 4. 失败是终止性的

`wal.sync()` 失败或状态写锁中毒时，队列记录 `failure: Option<String>`，清空 leader，`notify_all`。所有等待者收到 `TxnError::Writer`，后续提交在入队前也会被拒绝。M0 不做“重试某个提交”；fsync 失败后继续接受提交会把持久性语义弄乱。

**失败批的歧义**：这批 `Commit` 记录已经写进 WAL，只是没有确认 fsync。恢复时可能看到并重放它们，因此“客户端收到错误”不严格等于“恢复后一定不存在”。M0 不实现失败批的 WAL 截断/隔离；生产实现需要把失败批标记为不可恢复，或按 `valid_end` 截断并确保目录/文件系统语义。这个边界必须在验收里说清楚，不能把 `Writer` 错误当成完整回滚。

### 5. Checkpoint 要求没有活跃事务

`TxnStore::checkpoint` 先拿 WAL 锁，再检查 `active_txns == 0`，否则返回 `TxnError::ActiveTxns`。checkpoint 会重置 WAL；如果还有逻辑事务持有未提交的 `Begin`/`Put`，reset 会删掉它们，之后该事务再提交就会留下没有 `Begin` 的 `Commit`，恢复时报 `MissingBegin`。

`begin` 在持有 WAL 锁时增加 active 计数并追加 `Begin`；因此 checkpoint 检查通过后，新事务的 `Begin` 只能落在 reset 之后。`commit`/`abort`/`Drop` 在完成或放弃后减少计数。`Txn` 被丢弃时只减计数，不写 `Abort`；WAL 里留下 `Begin`/`Put`，恢复按“未提交”丢弃。

## 备选方案

### 每个事务一个线程，共享一个 writer 线程

会让 API 变成异步/通道式，`put` 的 LSN 和错误传播要重新设计。M0 先用锁 + 条件变量，保持同步 API。

### 固定 sleep 的组提交窗口

Windows 上 `sleep` 粒度可能是 1 ms 级，顺序提交会被固定延迟拖慢。改用 `yield_now()` 若干次，单提交只付调度点，不付毫秒级睡眠。

### 只组提交，不允许并发事务

做不到。没有并发事务，队列里永远只有一个 `CommitWork`，一次 fsync 只能服务一个事务。

### 在提交组里做冲突检测

需要读集/写集、版本号、回滚或等待。M0 只验证组提交和恢复；冲突语义推迟到 MVCC/快照隔离。

## 后果

好的方面：

- 1,024 事务 × 4 key、16 线程、3 轮：写总时间 median **867.780 ms**，fsync **260** 次（0.254 次/事务）；顺序提交（`txn-checkpoint` 的写事务阶段）**2,954.827 ms**，约 **3.40×** 提速。
- 256 事务 × 4 key、16 线程、3 轮：**242.826 ms**、**73** 次 fsync；顺序提交 **797.623 ms**，约 **3.29×**。
- `put` 仍返回 WAL LSN，CLI/bench 的现有调用不需要改。
- 读路径不拿 WAL 锁，不会被 `fsync` 直接阻塞；leader 应用阶段持有状态写锁，读者会被这一小段阻塞。

代价：

- WAL append 仍串行；提交组只摊薄 fsync，不提高 WAL 写入并行度。
- 没有冲突检测：同一 key 的并发写是“最后 WAL 顺序获胜”。
- `checkpoint` 不能与活跃事务重叠，仍是单写者停顿点。
- leader 会阻塞在 `wal.sync()` 和状态写锁上；组越大，单批延迟越高。
- 失败后 store 进入终止状态；没有自动恢复/重试，失败批可能仍出现在恢复结果里（见“失败批的歧义”）。
- 仍无跨进程锁、WAL 段轮转、后台 checkpoint。

## 验证

- `cargo test -p tritdb-txn`：14 个测试；新增 `checkpoint_rejects_active_transactions`、`concurrent_commits_share_fsyncs_and_recover`、`commit_group_applies_in_wal_order`。
- 并发测试连续 20 次：16 线程、16 个事务，`syncs < 16`，重开 `committed_txns == 16`、`discarded_txns == 0`。
- `cargo test --workspace`：M0 基线 158 个测试通过（M1 SQL/GROUP BY/INNER/CROSS/LEFT/RIGHT/FULL/NATURAL/USING JOIN/等值 hash join/EXPLAIN/EXPLAIN ANALYZE/ANALYZE 统计/逗号 FROM/UPDATE/谓词/CASE 切片后当前全量 239，见 README）。
- `cargo run -p tritdb-bench --release -- txn-group --txns 1024 --threads 16 --keys-per-txn 4 --rounds 3`。
