# TXNGC v0：并发逻辑事务与提交组

状态：**草案，未冻结**。实现位于 `crates/tritdb-txn`；决策记录见 ADR 006。

## 1. 边界

- 这是 `tritdb-txn` 的事务层行为，不是 TRWL 文件格式。
- 同一时刻只有一个 WAL 写者；`Mutex<WalWriter>` 只包住单次 append。
- 可以有多个逻辑事务同时存在；它们可以交错追加 `Begin/Put/Delete/Commit/Abort`。
- 不做冲突检测、不做 MVCC、不做快照隔离；同一 key 的并发写按 WAL 顺序最后者获胜。
- checkpoint 要求没有活跃逻辑事务；它不是后台 checkpoint。

## 2. 术语

- **逻辑事务**：`TxnStore::begin` 返回的 `Txn`；不持有 WAL 锁。
- **提交组**：一批已经追加 `Commit` 记录、等待同一次 fsync 的事务。
- **leader**：负责取走提交队列、执行一次 `WalWriter::sync()`、按 WAL 顺序应用状态并唤醒等待者的提交者。
- **seq**：提交队列内的单调序号；只用于等待/唤醒，不是 WAL LSN。
- **commit LSN**：该事务 `Commit` 记录的 WAL LSN。

## 3. 提交算法

`Txn::commit`：

1. 拿 `Mutex<WalWriter>`；
2. 追加 `Commit` 记录，`sync=false`；
3. 在仍持有 WAL 锁时，把 `CommitWork { seq, commit_lsn, ops }` 放进 `CommitQueue`；
4. 如果队列没有 leader，把自己设为 leader；否则释放 WAL 锁后等待；
5. 释放 WAL 锁。

leader：

1. `yield_now()` 32 次，给其他提交者入队的机会；
2. 拿 WAL 锁；
3. 拿队列锁，取走当前全部 `CommitWork`；
4. 调一次 `WalWriter::sync()`；
5. 按 `commit_lsn` 升序，把每个 work 的 ops 应用到 `RwLock<BTreeMap>`；
6. `synced_seq = max(seq)`，清空 leader，`notify_all`；
7. 释放队列锁、WAL 锁。

等待者：

1. 拿队列锁；
2. 若 `failure` 存在，返回 `TxnError::Writer`；
3. 若 `synced_seq >= my_seq`，返回；
4. 若没有 leader，把自己设为 leader，释放队列锁并执行 leader 流程；
5. 否则在 condvar 上等待。

## 4. 不变量

1. **入队与 WAL 追加同锁**：`Commit` 追加和入队都在 WAL 锁内完成。否则 leader 可能在 fsync 后取走队列，而某个已追加的 `Commit` 尚未入队；该事务会被误判为已同步，状态却没有应用。
2. **leader 取队列时同时持有 WAL 锁和队列锁**：新 append 不能在这期间发生，队列快照是完整的。
3. **一次 sync 覆盖整批 Commit**：leader 在 append 全部完成后调用 `sync_all`；等待者只有在该批 sync 成功且状态应用后才返回。
4. **状态按 commit LSN 应用**：WAL 恢复顺序和内存应用顺序一致；同一 key 的覆盖关系可复现。
5. **失败终止**：`sync` 失败或状态写锁中毒后，队列记录 `failure`，清 leader，唤醒全部等待者；后续提交在入队前拒绝。M0 不重试。
6. **checkpoint 与活跃事务互斥**：`checkpoint` 先拿 WAL 锁，再检查 `active_txns == 0`；否则返回 `ActiveTxns`，不 reset WAL。`begin` 在持有 WAL 锁时增加 active 计数并追加 `Begin`；因此检查后不会再插入一个会被 reset 丢掉的 `Begin`。

## 5. 锁顺序

- 普通 append：`WAL`。
- commit：`WAL -> commit queue`，然后释放 WAL，等待 `commit queue + condvar`。
- leader：`WAL -> commit queue -> state`。
- checkpoint：`WAL -> state`。
- 读者：`state`（读锁）。

没有 `state -> WAL` 或 `state -> commit queue` 的路径，因此不会与 leader 形成环。

## 6. 崩溃语义

- `Commit` 记录已追加但尚未 sync：崩溃后可能丢失；该事务没有持久化，符合“未提交丢弃”。
- `Commit` 记录已 sync、状态尚未应用：崩溃后恢复会重放该 `Commit`；内存状态在进程内可能短暂落后，但进程崩溃后由恢复补齐。
- 同一批中多个 `Commit` 已 sync：恢复按 WAL 顺序应用；不会出现半批状态。
- 快照 checkpoint 仍遵守“快照先持久化，再 reset WAL”；checkpoint 不与活跃事务重叠。

## 7. 错误

- `ActiveTxns { count }`：checkpoint 时有活跃事务。
- `Writer { reason }`：提交组 leader 失败，store 进入终止状态。
- `Poisoned`：WAL/队列/状态锁中毒。

## 8. 已验证

- 16 线程、16 个事务：`syncs < 16`，重开后 `committed_txns=16`、`discarded_txns=0`；连续 20 次通过。
- 同 key 两事务：按 WAL 顺序最后提交者获胜。
- 活跃事务下 checkpoint 返回 `ActiveTxns`。
- 1,024 事务 × 4 key、16 线程、3 轮：写总时间 median 867.780 ms，fsync 260 次（0.254 次/事务）。
- 全工作区 M0 基线 158 个测试通过（M1 SQL/GROUP BY/INNER/CROSS/LEFT/RIGHT/FULL/NATURAL/USING JOIN/等值 hash join/EXPLAIN/EXPLAIN ANALYZE/ANALYZE 统计/Auto join 代价/WHERE 合取项拆分与单源下推/逗号 FROM/UPDATE/谓词/CASE 切片后当前全量 242，见 README）。

## 9. 未做

- 组大小/单批延迟的在线调参；
- 写冲突检测与可串行化；
- 跨进程锁；
- WAL 段轮转；
- 后台 checkpoint；
- MVCC/快照隔离。
